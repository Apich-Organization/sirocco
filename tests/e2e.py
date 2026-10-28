#!/usr/bin/env python3
"""End-to-end acceptance tests for sirocco.

Compares trees with an independent implementation (hashlib) so the tool cannot
grade its own homework.
"""
import hashlib
import os
import random
import shutil
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "sirocco.exe")
if not os.path.exists(BIN):
    BIN = os.path.join(ROOT, "target", "release", "sirocco")
WORK = os.path.join(os.environ.get("TEMP", "/tmp"), "sirocco-e2e")

passed = 0
failed = 0
failures = []


def run(*args, expect=0, timeout=180):
    cmd = [BIN] + [str(a) for a in args]
    p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    if expect is not None and p.returncode != expect:
        raise AssertionError(
            f"{' '.join(cmd)} -> exit {p.returncode} (expected {expect})\n"
            f"stdout:\n{p.stdout}\nstderr:\n{p.stderr}"
        )
    return p


def check(name, fn):
    global passed, failed
    try:
        fn()
        passed += 1
        print(f"  ok   {name}")
    except Exception as e:
        failed += 1
        failures.append((name, e))
        print(f"  FAIL {name}: {e}")


def tree_digest(root):
    """Map relative path -> (size, sha256) for every file, plus the dir set."""
    files = {}
    dirs = set()
    for base, dirnames, filenames in os.walk(root):
        rel = os.path.relpath(base, root)
        if rel != ".":
            dirs.add(rel.replace("\\", "/"))
        for f in filenames:
            p = os.path.join(base, f)
            r = os.path.relpath(p, root).replace("\\", "/")
            h = hashlib.sha256()
            with open(p, "rb") as fh:
                while True:
                    b = fh.read(1 << 20)
                    if not b:
                        break
                    h.update(b)
            files[r] = (os.path.getsize(p), h.hexdigest())
    return files, dirs


def assert_same(a, b):
    fa, da = tree_digest(a)
    fb, db = tree_digest(b)
    if fa != fb:
        only_a = set(fa) - set(fb)
        only_b = set(fb) - set(fa)
        diff = [k for k in set(fa) & set(fb) if fa[k] != fb[k]]
        raise AssertionError(
            f"tree mismatch: only-src={sorted(only_a)[:5]} only-dst={sorted(only_b)[:5]} "
            f"differing={diff[:5]}"
        )
    if da != db:
        raise AssertionError(f"dir mismatch: {sorted(da ^ db)[:5]}")


def make_tree(root, seed=1):
    random.seed(seed)
    os.makedirs(root, exist_ok=True)
    os.makedirs(os.path.join(root, "sub"), exist_ok=True)
    os.makedirs(os.path.join(root, "deep", "a", "b"), exist_ok=True)
    os.makedirs(os.path.join(root, "empty_dir"), exist_ok=True)
    open(os.path.join(root, "empty.txt"), "wb").close()
    open(os.path.join(root, "tiny.txt"), "wb").write(b"hello world\n")
    open(os.path.join(root, "exact_4k.bin"), "wb").write(b"A" * 4096)
    open(os.path.join(root, "exact_1m.bin"), "wb").write(os.urandom(1 << 20))
    open(os.path.join(root, "odd_1m1.bin"), "wb").write(os.urandom((1 << 20) + 1))
    open(os.path.join(root, "sub", "text.txt"), "wb").write(
        b"the quick brown fox jumps over the lazy dog\n" * 100_000
    )
    open(os.path.join(root, "sub", "zeros.bin"), "wb").write(b"\0" * (6 << 20))
    open(os.path.join(root, "deep", "a", "b", "big.bin"), "wb").write(os.urandom(24 << 20))
    # many small files: exercises the small-file path and the handle cache
    small = os.path.join(root, "many")
    os.makedirs(small, exist_ok=True)
    for i in range(300):
        with open(os.path.join(small, f"f{i:04d}.dat"), "wb") as f:
            f.write(os.urandom(random.randint(0, 9000)))
    return root


def fresh(name):
    p = os.path.join(WORK, name)
    shutil.rmtree(p, ignore_errors=True)
    os.makedirs(p, exist_ok=True)
    return p


# --------------------------------------------------------------------- tests

def t_basic_copy():
    d = fresh("t1")
    src = make_tree(os.path.join(d, "src"))
    dst = os.path.join(d, "dst")
    run("copy", src, dst, "--verify=hash")
    assert_same(src, dst)


def t_copy_variants():
    src = os.path.join(WORK, "t1", "src")
    for i, extra in enumerate(
        [
            ["--no-direct"],
            ["--engine=blocking"],
            ["--sparse"],
            ["--order=physical"],
            ["--order=size-desc"],
            ["--buffer=4M", "--io-unit=4M"],
            ["--overlap=1"],
            ["--threads=2"],
            ["--verify=size-mtime"],
            ["--prealloc=off"],
            ["--prealloc=sparse"],
        ]
    ):
        dst = os.path.join(fresh(f"t2_{i}"), "dst")
        run("copy", src, dst, *extra)
        assert_same(src, dst)


def t_idempotent():
    src = os.path.join(WORK, "t1", "src")
    dst = os.path.join(fresh("t3"), "dst")
    run("copy", src, dst, "-q")
    p = run("copy", src, dst, "--json")
    if '"files_copied": 0' not in p.stdout:
        raise AssertionError(f"second run copied files again:\n{p.stdout}")


def t_incremental():
    src = os.path.join(WORK, "t1", "src")
    dst = os.path.join(WORK, "t3", "dst")
    # change one small file and one big one
    with open(os.path.join(src, "tiny.txt"), "wb") as f:
        f.write(b"changed!\n")
    with open(os.path.join(src, "deep", "a", "b", "big.bin"), "r+b") as f:
        f.seek(1024)
        f.write(b"XXXX")
    time.sleep(0.01)
    run("copy", src, dst, "--verify=hash", "-q")
    assert_same(src, dst)


def t_overwrite_policies():
    src = os.path.join(WORK, "t1", "src")
    dst = os.path.join(fresh("t4"), "dst")
    run("copy", src, dst, "-q")
    p = run("copy", src, dst, "--overwrite=never", "--json")
    if '"files_copied": 0' not in p.stdout:
        raise AssertionError("overwrite=never still copied")
    p = run("copy", src, dst, "--overwrite=always", "--json")
    if '"files_skipped": 0' not in p.stdout:
        raise AssertionError("overwrite=always skipped files")
    run("copy", src, dst, "--overwrite=if-content", "-q")
    assert_same(src, dst)


def t_move():
    d = fresh("t5")
    src = make_tree(os.path.join(d, "src"))
    ref, _ = tree_digest(src)
    dst = os.path.join(d, "dst")
    run("move", src, dst, "-q")
    got, _ = tree_digest(dst)
    if got != ref:
        raise AssertionError("move produced a different tree")
    if os.path.exists(os.path.join(src, "tiny.txt")):
        raise AssertionError("move left the source behind")


def t_delete():
    src = make_tree(os.path.join(fresh("t6"), "src"))
    run("delete", src, "-q")
    if os.path.exists(src):
        raise AssertionError("delete left the tree behind")


def t_sync_delete_extra():
    d = fresh("t7")
    src = make_tree(os.path.join(d, "src"))
    dst = os.path.join(d, "dst")
    run("copy", src, dst, "-q")
    os.remove(os.path.join(src, "tiny.txt"))
    shutil.rmtree(os.path.join(src, "many"))
    run("sync", src, dst, "-q")
    assert_same(src, dst)


def t_exclude():
    src = os.path.join(WORK, "t1", "src")
    dst = os.path.join(fresh("t8"), "dst")
    run("copy", src, dst, "--exclude=*.bin", "--exclude=many", "-q")
    got, _ = tree_digest(dst)
    if any(k.endswith(".bin") for k in got):
        raise AssertionError("exclude did not remove .bin files")
    if any(k.startswith("many/") for k in got):
        raise AssertionError("exclude did not remove the many/ directory")


def t_single_file():
    src = os.path.join(WORK, "t1", "src", "odd_1m1.bin")
    dst = os.path.join(fresh("t9"), "one.bin")
    run("copy", src, dst, "--verify=hash", "-q")
    a = hashlib.sha256(open(src, "rb").read()).hexdigest()
    b = hashlib.sha256(open(dst, "rb").read()).hexdigest()
    if a != b:
        raise AssertionError("single file content differs")


def t_append_delta():
    """Growing file + rewritten tail should be updated correctly."""
    d = fresh("t10")
    src = os.path.join(d, "src")
    dst = os.path.join(d, "dst")
    os.makedirs(src)
    os.makedirs(dst)
    base = os.urandom(24 << 20)
    with open(os.path.join(src, "log.bin"), "wb") as f:
        f.write(base)
    run("copy", src, dst, "-q")
    # append 8 MiB
    with open(os.path.join(src, "log.bin"), "ab") as f:
        f.write(os.urandom(8 << 20))
    run("copy", src, dst, "--verify=hash", "-v", "-q")
    assert_same(src, dst)
    # rewrite a region in the middle
    with open(os.path.join(src, "log.bin"), "r+b") as f:
        f.seek(4 << 20)
        f.write(os.urandom(2 << 20))
    run("copy", src, dst, "--verify=hash", "-q")
    assert_same(src, dst)


def t_mtime_preserved():
    src = os.path.join(WORK, "t1", "src", "tiny.txt")
    dst = os.path.join(fresh("t11"), "tiny.txt")
    run("copy", src, dst, "-q")
    a = os.path.getmtime(src)
    b = os.path.getmtime(dst)
    if abs(a - b) > 2:
        raise AssertionError(f"mtime not preserved: {a} vs {b}")


def t_verify_command():
    src = os.path.join(WORK, "t1", "src")
    dst = os.path.join(WORK, "t3", "dst")
    run("verify", src, dst, "-q", expect=0)
    with open(os.path.join(dst, "tiny.txt"), "ab") as f:
        f.write(b"corrupt")
    run("verify", src, dst, "-q", expect=1)


def t_big_file():
    d = fresh("t12")
    src = os.path.join(d, "src")
    os.makedirs(src)
    p = os.path.join(src, "huge.bin")
    with open(p, "wb") as f:
        for _ in range(128):
            f.write(os.urandom(1 << 20))
    dst = os.path.join(d, "dst")
    run("copy", src, dst, "--verify=hash", "-q")
    assert_same(src, dst)


def t_into_semantics():
    d = fresh("t13")
    src = make_tree(os.path.join(d, "src"))
    dst = os.path.join(d, "dst")
    os.makedirs(dst)
    # Default: merge the contents of src into dst.
    run("copy", src, dst, "-q")
    if not os.path.exists(os.path.join(dst, "tiny.txt")):
        raise AssertionError("default copy did not merge contents")
    # --into: cp -r semantics, the source directory itself lands inside dst.
    dst2 = os.path.join(d, "dst2")
    os.makedirs(dst2)
    run("copy", src, dst2, "--into", "-q")
    if not os.path.exists(os.path.join(dst2, "src", "tiny.txt")):
        raise AssertionError("--into did not nest the source directory")


def t_exclude_from_file():
    d = fresh("t14")
    src = os.path.join(WORK, "t1", "src")
    dst = os.path.join(d, "dst")
    pat = os.path.join(d, "patterns.txt")
    with open(pat, "w") as f:
        f.write("# comment\n*.bin\n\n")
    run("copy", src, dst, "--exclude-from", pat, "-q")
    got, _ = tree_digest(dst)
    if any(k.endswith(".bin") for k in got):
        raise AssertionError("--exclude-from did not filter")


def t_index_delta():
    d = fresh("t15")
    src = os.path.join(d, "src")
    dst = os.path.join(d, "dst")
    idx = os.path.join(d, "idx")
    os.makedirs(src)
    data = bytearray(os.urandom(24 << 20))
    with open(os.path.join(src, "img.bin"), "wb") as f:
        f.write(data)
    run("copy", src, dst, "--index", "--index-dir", idx, "-q")
    # change one 1 MiB block
    data[8 << 20 : 9 << 20] = os.urandom(1 << 20)
    with open(os.path.join(src, "img.bin"), "wb") as f:
        f.write(data)
    p = run("copy", src, dst, "--index", "--index-dir", idx, "--json")
    assert_same(src, dst)
    written = int(p.stdout.split('"bytes_written": ')[1].split(",")[0])
    if written > (3 << 20):
        raise AssertionError(f"indexed delta wrote {written} bytes for a 1 MiB change")


def t_small_copy_modes():
    d = fresh("t16")
    src = os.path.join(WORK, "t1", "src")
    for i, mode in enumerate(["kernel", "io"]):
        dst = os.path.join(d, f"dst{i}")
        run("copy", src, dst, "--small-copy", mode, "-q")
        assert_same(src, dst)


def t_sparse_source():
    """Unallocated regions of the source must be punched, not copied."""
    d = fresh("t17")
    src = os.path.join(d, "src")
    dst = os.path.join(d, "dst")
    os.makedirs(src)
    path = os.path.join(src, "img.bin")
    with open(path, "wb") as f:
        f.truncate(48 << 20)
        f.seek(8 << 20)
        f.write(os.urandom(1 << 20))
        f.seek(32 << 20)
        f.write(os.urandom(1 << 20))
    p = run("copy", src, dst, "--sparse", "--json")
    assert_same(src, dst)
    saved = int(p.stdout.split('"bytes_saved": ')[1].split(",")[0])
    if saved < (40 << 20):
        raise AssertionError(f"only {saved} bytes punched out of 48 MiB")


def t_split_processes():
    d = fresh("t18")
    src = os.path.join(WORK, "t1", "src")
    dst = os.path.join(d, "dst")
    run("copy", src, dst, "--procs=4", "-q")
    assert_same(src, dst)


def t_acl_flag():
    d = fresh("t19")
    src = make_tree(os.path.join(d, "src"))
    dst = os.path.join(d, "dst")
    run("copy", src, dst, "--acl", "-q")
    assert_same(src, dst)


TESTS = [
    ("basic copy", t_basic_copy),
    ("copy variants", t_copy_variants),
    ("idempotent rerun", t_idempotent),
    ("incremental update", t_incremental),
    ("overwrite policies", t_overwrite_policies),
    ("move", t_move),
    ("delete", t_delete),
    ("sync deletes extras", t_sync_delete_extra),
    ("exclude patterns", t_exclude),
    ("single file", t_single_file),
    ("append + delta", t_append_delta),
    ("mtime preserved", t_mtime_preserved),
    ("verify command", t_verify_command),
    ("128 MiB file", t_big_file),
    ("into semantics", t_into_semantics),
    ("exclude-from file", t_exclude_from_file),
    ("indexed delta", t_index_delta),
    ("small-copy modes", t_small_copy_modes),
    ("sparse source holes", t_sparse_source),
    ("multi-process split", t_split_processes),
    ("acl flag", t_acl_flag),
]


def main():
    if not os.path.exists(BIN):
        print(f"binary not found: {BIN}")
        return 1
    print(f"sirocco e2e tests using {BIN}\n")
    only = sys.argv[1] if len(sys.argv) > 1 else None
    for name, fn in TESTS:
        if only and only not in name:
            continue
        check(name, fn)
    print(f"\n{passed} passed, {failed} failed")
    for name, e in failures:
        print(f"\n--- {name}\n{e}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
