"""Launch the GUI with an auto-started job and capture its client area.

Polling for the window instead of a fixed sleep: the first frame takes a couple of
seconds on a cold shader cache, and a fixed sleep either misses the window or
misses the job.
"""

import ctypes
import ctypes.wintypes as wt
import subprocess
import sys
import time

from PIL import ImageGrab

EXE = r"D:/dev/fc-rs/target/release/sirocco-gui.exe"


def find_window(timeout=25.0):
    u = ctypes.windll.user32
    deadline = time.time() + timeout
    while time.time() < deadline:
        hits = []

        def cb(h, _l):
            if u.IsWindowVisible(h):
                n = u.GetWindowTextLengthW(h)
                if n:
                    b = ctypes.create_unicode_buffer(n + 1)
                    u.GetWindowTextW(h, b, n + 1)
                    if b.value.startswith("Sirocco"):
                        hits.append(h)
            return True

        u.EnumWindows(ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)(cb), 0)
        if hits:
            return hits[0]
        time.sleep(0.2)
    return None


def capture(hwnd, path):
    u = ctypes.windll.user32
    # A synthetic ALT tap unlocks SetForegroundWindow for the calling process.
    u.keybd_event(0x12, 0, 0, 0)
    u.keybd_event(0x12, 0, 2, 0)
    # Park it at the origin: the window is remembered between runs and a
    # partially offscreen window makes the capture include desktop pixels.
    SWP_NOSIZE, SWP_SHOWWINDOW = 0x0001, 0x0040
    u.SetWindowPos(hwnd, 0, 0, 0, 0, 0, SWP_NOSIZE | SWP_SHOWWINDOW)
    u.SetForegroundWindow(hwnd)
    time.sleep(0.5)
    r = wt.RECT()
    u.GetClientRect(hwnd, ctypes.byref(r))
    p = wt.POINT(0, 0)
    u.ClientToScreen(hwnd, ctypes.byref(p))
    img = ImageGrab.grab(bbox=(p.x, p.y, p.x + r.right, p.y + r.bottom))
    img.save(path)
    return img.size


def main():
    src, dst = sys.argv[1], sys.argv[2]
    out = sys.argv[3] if len(sys.argv) > 3 else "D:/dev/fc-rs/target/shot.png"
    wait = float(sys.argv[4]) if len(sys.argv) > 4 else 0.0

    subprocess.run(["taskkill", "/IM", "sirocco-gui.exe", "/F"], capture_output=True)
    time.sleep(0.6)
    subprocess.run(["rm", "-rf", dst], capture_output=True)
    env = {
        "SIROCCO_GUI_ONTOP": "1",
        "SIROCCO_GUI_SRC": src,
        "SIROCCO_GUI_DST": dst,
    }
    import os

    subprocess.Popen([EXE], env={**os.environ, **env}, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    hwnd = find_window()
    if not hwnd:
        print("no window")
        return 1
    if wait:
        time.sleep(wait)
    print("captured", capture(hwnd, out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
