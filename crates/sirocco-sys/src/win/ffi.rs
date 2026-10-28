//! Hand-written Win32/NT bindings.
//!
//! Deliberately not `windows-sys`: we want exactly these entry points, the exact
//! ABI, and nothing else. Everything is `unsafe extern "system"` with the
//! documented signatures.

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]
#![allow(clippy::upper_case_acronyms)]
// A transcription of the system headers: the identifier *is* the
// documentation, and a doc comment per constant would only repeat it.
#![allow(missing_docs)]

use core::ffi::c_void;

pub type HANDLE = *mut c_void;
pub type BOOL = i32;
pub type DWORD = u32;
pub type ULONG = u32;
pub type ULONG_PTR = usize;
pub type SIZE_T = usize;
pub type LPVOID = *mut c_void;
pub type LPCVOID = *const c_void;
pub type LPDWORD = *mut u32;
pub type LONGLONG = i64;
pub type BOOLEAN = u8;

pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;
pub const TRUE: BOOL = 1;
pub const FALSE: BOOL = 0;
pub const INFINITE: u32 = 0xFFFF_FFFF;

// ---------------------------------------------------------------- access rights
pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const GENERIC_ALL: u32 = 0x1000_0000;
pub const DELETE: u32 = 0x0001_0000;
pub const FILE_READ_ATTRIBUTES: u32 = 0x0080;
pub const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
pub const READ_CONTROL: u32 = 0x0002_0000;
pub const SYNCHRONIZE: u32 = 0x0010_0000;
pub const WRITE_DAC: u32 = 0x0004_0000;
pub const WRITE_OWNER: u32 = 0x0008_0000;

// ------------------------------------------------------------------- share mode
pub const FILE_SHARE_READ: u32 = 0x1;
pub const FILE_SHARE_WRITE: u32 = 0x2;
pub const FILE_SHARE_DELETE: u32 = 0x4;

// ----------------------------------------------------------- creation disposition
pub const CREATE_NEW: u32 = 1;
pub const CREATE_ALWAYS: u32 = 2;
pub const OPEN_EXISTING: u32 = 3;
pub const OPEN_ALWAYS: u32 = 4;
pub const TRUNCATE_EXISTING: u32 = 5;

// ------------------------------------------------------------------ file flags
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
pub const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0080;
pub const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
pub const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
pub const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
pub const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
pub const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x4000;
pub const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x40000;
pub const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
pub const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
pub const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
pub const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
pub const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
pub const FILE_FLAG_RANDOM_ACCESS: u32 = 0x1000_0000;
pub const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
pub const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
pub const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
pub const FILE_FLAG_POSIX_SEMANTICS: u32 = 0x0100_0000;

// -------------------------------------------------------------- error numbers
pub const ERROR_SUCCESS: u32 = 0;
pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_INVALID_HANDLE: u32 = 6;
pub const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
pub const ERROR_HANDLE_EOF: u32 = 38;
pub const ERROR_NOT_SUPPORTED: u32 = 50;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
pub const ERROR_MORE_DATA: u32 = 234;
pub const ERROR_IO_PENDING: u32 = 997;
pub const ERROR_IO_INCOMPLETE: u32 = 996;
pub const ERROR_WAIT_TIMEOUT: u32 = 258;
pub const ERROR_NOT_SAME_DEVICE: u32 = 17;
pub const ERROR_ALREADY_EXISTS: u32 = 183;
pub const ERROR_DISK_FULL: u32 = 112;
pub const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;
pub const ERROR_NO_SYSTEM_RESOURCES: u32 = 1450;
pub const ERROR_FILE_TOO_LARGE: u32 = 223;

// --------------------------------------------------------------- memory flags
pub const MEM_COMMIT: u32 = 0x0000_1000;
pub const MEM_RESERVE: u32 = 0x0000_2000;
pub const MEM_DECOMMIT: u32 = 0x0000_4000;
pub const MEM_RELEASE: u32 = 0x0000_8000;
pub const MEM_LARGE_PAGES: u32 = 0x2000_0000;
pub const MEM_PHYSICAL: u32 = 0x0040_0000;
pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_NOACCESS: u32 = 0x01;

// ------------------------------------------------------------- priorities
pub const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
pub const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
pub const NORMAL_PRIORITY_CLASS: u32 = 0x0000_0020;
pub const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;
pub const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;
pub const REALTIME_PRIORITY_CLASS: u32 = 0x0000_0100;

pub const THREAD_PRIORITY_IDLE: i32 = -15;
pub const THREAD_PRIORITY_LOWEST: i32 = -2;
pub const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
pub const THREAD_PRIORITY_NORMAL: i32 = 0;
pub const THREAD_PRIORITY_ABOVE_NORMAL: i32 = 1;
pub const THREAD_PRIORITY_HIGHEST: i32 = 2;
pub const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;

// ------------------------------------------------------------- privileges
pub const TOKEN_QUERY: u32 = 0x0008;
pub const TokenElevation: u32 = 20;
pub const TokenElevationType: u32 = 18;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TOKEN_ELEVATION {
    pub TokenIsElevated: u32,
}
pub const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
pub const SE_PRIVILEGE_ENABLED: u32 = 0x0000_0002;
pub const SE_PRIVILEGE_ENABLED_BY_DEFAULT: u32 = 0x0000_0001;

// ------------------------------------------------------------- thread info
pub const THREAD_PRIORITY_ERROR_RETURN: i32 = 0x7FFF_FFFF;

// ------------------------------------------------------------- CTL_CODE helpers
#[inline]
#[must_use]
pub const fn ctl_code(
    dev: u32,
    func: u32,
    method: u32,
    access: u32,
) -> u32 {
    (dev << 16) | (access << 14) | (func << 2) | method
}

pub const FILE_DEVICE_FILE_SYSTEM: u32 = 0x0000_0009;
pub const FILE_DEVICE_DISK: u32 = 0x0000_0007;
pub const FILE_DEVICE_MASS_STORAGE: u32 = 0x0000_002d;
pub const METHOD_BUFFERED: u32 = 0;
pub const METHOD_IN_DIRECT: u32 = 1;
pub const METHOD_OUT_DIRECT: u32 = 2;
pub const METHOD_NEITHER: u32 = 3;
pub const FILE_ANY_ACCESS: u32 = 0;
pub const FILE_READ_ACCESS: u32 = 1;
pub const FILE_WRITE_ACCESS: u32 = 2;

pub const FSCTL_SET_SPARSE: u32 =
    ctl_code(FILE_DEVICE_FILE_SYSTEM, 49, METHOD_BUFFERED, FILE_SPECIAL);
pub const FSCTL_SET_ZERO_DATA: u32 = ctl_code(
    FILE_DEVICE_FILE_SYSTEM,
    50,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);
pub const FSCTL_SET_COMPRESSION: u32 = ctl_code(
    FILE_DEVICE_FILE_SYSTEM,
    16,
    METHOD_BUFFERED,
    FILE_READ_ACCESS | FILE_WRITE_ACCESS,
);
pub const FSCTL_GET_RETRIEVAL_POINTERS: u32 =
    ctl_code(FILE_DEVICE_FILE_SYSTEM, 28, METHOD_NEITHER, FILE_ANY_ACCESS);
pub const FSCTL_QUERY_ALLOCATED_RANGES: u32 = ctl_code(
    FILE_DEVICE_FILE_SYSTEM,
    51,
    METHOD_NEITHER,
    FILE_READ_ACCESS,
);
pub const FSCTL_GET_COMPRESSION: u32 = ctl_code(
    FILE_DEVICE_FILE_SYSTEM,
    15,
    METHOD_BUFFERED,
    FILE_ANY_ACCESS,
);
pub const IOCTL_STORAGE_QUERY_PROPERTY: u32 = ctl_code(
    FILE_DEVICE_MASS_STORAGE,
    0x0500,
    METHOD_BUFFERED,
    FILE_ANY_ACCESS,
);
pub const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 =
    ctl_code(FILE_DEVICE_DISK, 0x28, METHOD_BUFFERED, FILE_ANY_ACCESS);

const FILE_SPECIAL: u32 = 0;

pub const COMPRESSION_FORMAT_DEFAULT: u16 = 1;
pub const COMPRESSION_FORMAT_NONE: u16 = 0;

// ------------------------------------------------------------- structures
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SECURITY_ATTRIBUTES {
    pub nLength: u32,
    pub lpSecurityDescriptor: LPVOID,
    pub bInheritHandle: BOOL,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct OVERLAPPED_S {
    pub Offset: u32,
    pub OffsetHigh: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union OVERLAPPED_U {
    pub s: OVERLAPPED_S,
    pub Pointer: LPVOID,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OVERLAPPED {
    pub Internal: ULONG_PTR,
    pub InternalHigh: ULONG_PTR,
    pub u: OVERLAPPED_U,
    pub hEvent: HANDLE,
}

impl Default for OVERLAPPED {
    fn default() -> Self {
        Self {
            Internal: 0,
            InternalHigh: 0,
            u: OVERLAPPED_U {
                s: OVERLAPPED_S {
                    Offset: 0,
                    OffsetHigh: 0,
                },
            },
            hEvent: core::ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OVERLAPPED_ENTRY {
    pub lpCompletionKey: ULONG_PTR,
    pub lpOverlapped: *mut OVERLAPPED,
    pub Internal: ULONG_PTR,
    pub dwNumberOfBytesTransferred: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILETIME {
    pub dwLowDateTime: u32,
    pub dwHighDateTime: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SYSTEM_INFO {
    pub wProcessorArchitecture: u16,
    pub wReserved: u16,
    pub dwPageSize: u32,
    pub lpMinimumApplicationAddress: LPVOID,
    pub lpMaximumApplicationAddress: LPVOID,
    pub dwActiveProcessorMask: ULONG_PTR,
    pub dwNumberOfProcessors: u32,
    pub dwProcessorType: u32,
    pub dwAllocationGranularity: u32,
    pub wProcessorLevel: u16,
    pub wProcessorRevision: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GROUP_AFFINITY {
    pub Mask: ULONG_PTR,
    pub Group: u16,
    pub Reserved: [u16; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PROCESSOR_NUMBER {
    pub Group: u16,
    pub Number: u8,
    pub Reserved: u8,
}

#[repr(C)]
pub struct TOKEN_PRIVILEGES {
    pub PrivilegeCount: u32,
    pub Privileges: [LUID_AND_ATTRIBUTES; 1],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LUID {
    pub LowPart: u32,
    pub HighPart: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LUID_AND_ATTRIBUTES {
    pub Luid: LUID,
    pub Attributes: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILE_ZERO_DATA_INFORMATION {
    pub FileOffset: i64,
    pub BeyondFinalZero: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILE_ALLOCATED_RANGE_BUFFER {
    pub FileOffset: i64,
    pub Length: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct STORAGE_PROPERTY_QUERY {
    pub PropertyId: u32,
    pub QueryType: u32,
    pub AdditionalParameters: [u8; 1],
}

pub const StorageDeviceSeekPenaltyProperty: u32 = 7;
pub const StorageAccessAlignmentProperty: u32 = 6;
pub const PropertyStandardQuery: u32 = 0;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DEVICE_SEEK_PENALTY_DESCRIPTOR {
    pub Version: u32,
    pub Size: u32,
    pub IncursSeekPenalty: BOOLEAN,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR {
    pub Version: u32,
    pub Size: u32,
    pub BytesPerCacheLine: u32,
    pub BytesOffsetForCacheAlignment: u32,
    pub BytesPerLogicalSector: u32,
    pub BytesPerPhysicalSector: u32,
    pub BytesOffsetForSectorAlignment: u32,
}

/// `FILE_IO_PRIORITY_HINT_INFO` — used with `SetFileInformationByHandle`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILE_IO_PRIORITY_HINT_INFO {
    pub PriorityHint: u32,
}

pub const IoPriorityVeryLow: u32 = 0;
pub const IoPriorityLow: u32 = 1;
pub const IoPriorityNormal: u32 = 2;
pub const IoPriorityHigh: u32 = 3;
pub const IoPriorityCritical: u32 = 4;

pub const FileIoPriorityHintInfo: u32 = 12;
pub const FileIdInfo: u32 = 18;
pub const FileEndOfFileInfo: u32 = 6;
pub const FileBasicInfo: u32 = 0;
pub const FileStandardInfo: u32 = 1;

// `RETRIEVAL_POINTERS_BUFFER` header: ExtentCount, StartingVcn, then extents.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RETRIEVAL_POINTERS_BUFFER_HEAD {
    pub ExtentCount: u32,
    pub StartingVcn: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MAPPING_PAIR {
    pub NextVcn: i64,
    pub Lcn: i64,
}

// ------------------------------------------------------------- logical processor
pub const RelationProcessorCore: u32 = 0;
pub const RelationNumaNode: u32 = 1;
pub const RelationCache: u32 = 2;
pub const RelationProcessorPackage: u32 = 3;
pub const RelationGroup: u32 = 4;
pub const RelationAll: u32 = 0xFFFF;

#[repr(C)]
pub struct LOGICAL_PROCESSOR_RELATIONSHIP {
    pub Relationship: u32,
    pub Size: u32,
    // Variable-length payload follows.
}

// ------------------------------------------------------------------ imports
#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn CreateFileW(
        lpFileName: *const u16,
        dwDesiredAccess: u32,
        dwShareMode: u32,
        lpSecurityAttributes: *const SECURITY_ATTRIBUTES,
        dwCreationDisposition: u32,
        dwFlagsAndAttributes: u32,
        hTemplateFile: HANDLE,
    ) -> HANDLE;

    pub fn CloseHandle(hObject: HANDLE) -> BOOL;
    pub fn ReadFile(
        hFile: HANDLE,
        lpBuffer: LPVOID,
        nNumberOfBytesToRead: u32,
        lpNumberOfBytesRead: LPDWORD,
        lpOverlapped: *mut OVERLAPPED,
    ) -> BOOL;
    pub fn WriteFile(
        hFile: HANDLE,
        lpBuffer: LPCVOID,
        nNumberOfBytesToWrite: u32,
        lpNumberOfBytesWritten: LPDWORD,
        lpOverlapped: *mut OVERLAPPED,
    ) -> BOOL;

    pub fn GetLastError() -> u32;
    pub fn GetFileSizeEx(
        hFile: HANDLE,
        lpFileSize: *mut i64,
    ) -> BOOL;
    pub fn SetFilePointerEx(
        hFile: HANDLE,
        liDistanceToMove: i64,
        lpNewFilePointer: *mut i64,
        dwMoveMethod: u32,
    ) -> BOOL;
    pub fn SetEndOfFile(hFile: HANDLE) -> BOOL;
    pub fn SetFileValidData(
        hFile: HANDLE,
        ValidDataLength: i64,
    ) -> BOOL;
    pub fn SetFileTime(
        hFile: HANDLE,
        lpCreationTime: *const FILETIME,
        lpLastAccessTime: *const FILETIME,
        lpLastWriteTime: *const FILETIME,
    ) -> BOOL;
    pub fn FlushFileBuffers(hFile: HANDLE) -> BOOL;

    pub fn DeviceIoControl(
        hDevice: HANDLE,
        dwIoControlCode: u32,
        lpInBuffer: LPVOID,
        nInBufferSize: u32,
        lpOutBuffer: LPVOID,
        nOutBufferSize: u32,
        lpBytesReturned: LPDWORD,
        lpOverlapped: *mut OVERLAPPED,
    ) -> BOOL;

    pub fn GetFileInformationByHandleEx(
        hFile: HANDLE,
        FileInformationClass: u32,
        lpFileInformation: LPVOID,
        dwBufferSize: u32,
    ) -> BOOL;
    pub fn SetFileInformationByHandle(
        hFile: HANDLE,
        FileInformationClass: u32,
        lpFileInformation: LPVOID,
        dwBufferSize: u32,
    ) -> BOOL;

    pub fn CreateIoCompletionPort(
        FileHandle: HANDLE,
        ExistingCompletionPort: HANDLE,
        CompletionKey: ULONG_PTR,
        NumberOfConcurrentThreads: u32,
    ) -> HANDLE;
    pub fn GetQueuedCompletionStatusEx(
        CompletionPort: HANDLE,
        lpCompletionPortEntries: *mut OVERLAPPED_ENTRY,
        ulCount: u32,
        ulNumEntriesRemoved: *mut u32,
        dwMilliseconds: u32,
        fAlertable: BOOL,
    ) -> BOOL;
    pub fn GetOverlappedResult(
        hFile: HANDLE,
        lpOverlapped: *mut OVERLAPPED,
        lpNumberOfBytesTransferred: LPDWORD,
        bWait: BOOL,
    ) -> BOOL;
    pub fn CancelIoEx(
        hFile: HANDLE,
        lpOverlapped: *mut OVERLAPPED,
    ) -> BOOL;

    pub fn GetDiskFreeSpaceW(
        lpRootPathName: *const u16,
        lpSectorsPerCluster: LPDWORD,
        lpBytesPerSector: LPDWORD,
        lpNumberOfFreeClusters: LPDWORD,
        lpTotalNumberOfClusters: LPDWORD,
    ) -> BOOL;
    pub fn GetVolumePathNameW(
        lpszFileName: *const u16,
        lpszVolumePathName: *mut u16,
        cchBufferLength: u32,
    ) -> BOOL;
    pub fn GetVolumeInformationW(
        lpRootPathName: *const u16,
        lpVolumeNameBuffer: *mut u16,
        nVolumeNameSize: u32,
        lpVolumeSerialNumber: LPDWORD,
        lpMaximumComponentLength: LPDWORD,
        lpFileSystemFlags: LPDWORD,
        lpFileSystemNameBuffer: *mut u16,
        nFileSystemNameSize: u32,
    ) -> BOOL;

    pub fn VirtualAlloc(
        lpAddress: LPVOID,
        dwSize: SIZE_T,
        flAllocationType: u32,
        flProtect: u32,
    ) -> LPVOID;
    pub fn VirtualFree(
        lpAddress: LPVOID,
        dwSize: SIZE_T,
        dwFreeType: u32,
    ) -> BOOL;
    pub fn VirtualLock(
        lpAddress: LPVOID,
        dwSize: SIZE_T,
    ) -> BOOL;
    pub fn DiscardVirtualMemory(
        VirtualAddress: LPVOID,
        Size: SIZE_T,
    ) -> u32;
    pub fn GetLargePageMinimum() -> SIZE_T;
    pub fn GetSystemInfo(lpSystemInfo: *mut SYSTEM_INFO);

    pub fn LocalFree(hMem: LPVOID) -> LPVOID;

    pub fn CreateFileMappingW(
        hFile: HANDLE,
        lpFileMappingAttributes: *const SECURITY_ATTRIBUTES,
        flProtect: u32,
        dwMaximumSizeHigh: u32,
        dwMaximumSizeLow: u32,
        lpName: *const u16,
    ) -> HANDLE;
    pub fn MapViewOfFile(
        hFileMappingObject: HANDLE,
        dwDesiredAccess: u32,
        dwFileOffsetHigh: u32,
        dwFileOffsetLow: u32,
        dwNumberOfBytesToMap: SIZE_T,
    ) -> LPVOID;
    pub fn UnmapViewOfFile(lpBaseAddress: LPCVOID) -> BOOL;

    pub fn GetDiskFreeSpaceExW(
        lpDirectoryName: *const u16,
        lpFreeBytesAvailableToCaller: *mut u64,
        lpTotalNumberOfBytes: *mut u64,
        lpTotalNumberOfFreeBytes: *mut u64,
    ) -> BOOL;

    pub fn GetCurrentProcess() -> HANDLE;
    pub fn GetCurrentThread() -> HANDLE;
    pub fn SetPriorityClass(
        hProcess: HANDLE,
        dwPriorityClass: u32,
    ) -> BOOL;
    pub fn SetThreadPriority(
        hThread: HANDLE,
        nPriority: i32,
    ) -> BOOL;
    pub fn SetThreadGroupAffinity(
        hThread: HANDLE,
        ConstantGroupAffinity: *const GROUP_AFFINITY,
        PreviousGroupAffinity: *mut GROUP_AFFINITY,
    ) -> BOOL;
    pub fn GetActiveProcessorCount(GroupNumber: u16) -> u32;
    pub fn GetModuleHandleW(lpModuleName: *const u16) -> HANDLE;
    pub fn GetProcAddress(
        hModule: HANDLE,
        lpProcName: *const u8,
    ) -> *const core::ffi::c_void;
    pub fn VirtualAllocExNuma(
        hProcess: HANDLE,
        lpAddress: LPVOID,
        dwSize: SIZE_T,
        flAllocationType: u32,
        flProtect: u32,
        nndPreferred: u32,
    ) -> LPVOID;
    pub fn GetLogicalProcessorInformationEx(
        RelationshipType: u32,
        Buffer: *mut u8,
        ReturnedLength: *mut u32,
    ) -> BOOL;
    pub fn GetCurrentProcessorNumber() -> u32;
    pub fn CopyFile2(
        pwszExistingFileName: *const u16,
        pwszNewFileName: *const u16,
        pExtendedParameters: *const COPYFILE2_EXTENDED_PARAMETERS,
    ) -> i32;
    pub fn CreateHardLinkW(
        lpFileName: *const u16,
        lpExistingFileName: *const u16,
        lpSecurityAttributes: *const SECURITY_ATTRIBUTES,
    ) -> BOOL;
    pub fn CopyFileExW(
        lpExistingFileName: *const u16,
        lpNewFileName: *const u16,
        lpProgressRoutine: *mut core::ffi::c_void,
        lpData: *mut core::ffi::c_void,
        pbCancel: *mut BOOL,
        dwCopyFlags: u32,
    ) -> BOOL;
    pub fn SetFileAttributesW(
        lpFileName: *const u16,
        dwFileAttributes: u32,
    ) -> BOOL;
    pub fn GetFileAttributesW(lpFileName: *const u16) -> u32;
}

// ---------------------------------------------------------------- NT native
pub const FileStandardInformation: u32 = 5;
pub const FileEndOfFileInformation: u32 = 20;
pub const FileLinkInformation: u32 = 11;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IO_STATUS_BLOCK {
    pub status: usize,
    pub information: usize,
}

// ---------------------------------------------------------------- CopyFile2
pub const COPY_FILE_FAIL_IF_EXISTS: u32 = 0x1;
pub const COPY_FILE_COPY_SYMLINK: u32 = 0x800;
pub const COPY_FILE_NO_BUFFERING: u32 = 0x1000;
// From winbase.h — the values are not in ascending order, which is exactly how
// they get transcribed wrong.
pub const COPY_FILE_REQUEST_COMPRESSED_TRAFFIC: u32 = 0x1000_0000;
pub const COPY_FILE_REQUEST_SECURITY_PRIVILEGES: u32 = 0x2000;
pub const COPY_FILE_RESTARTABLE: u32 = 0x2000_0000;

/// `COPYFILE2_EXTENDED_PARAMETERS`, version 1 (32 bytes on x64).
#[repr(C)]
pub struct COPYFILE2_EXTENDED_PARAMETERS {
    pub dwSize: u32,
    pub dwCopyFlags: u32,
    pub pfCancel: *mut i32,
    pub pProgressRoutine: *mut core::ffi::c_void,
    pub pvCallbackContext: *mut core::ffi::c_void,
}

pub const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
pub const MOVEFILE_COPY_ALLOWED: u32 = 0x2;
pub const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

pub const SE_FILE_OBJECT: u32 = 1;
pub const OWNER_SECURITY_INFORMATION: u32 = 0x1;
pub const GROUP_SECURITY_INFORMATION: u32 = 0x2;
pub const DACL_SECURITY_INFORMATION: u32 = 0x4;

pub const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x1000;
pub const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x200;
pub const FORMAT_MESSAGE_ALLOCATE_BUFFER: u32 = 0x100;

#[link(name = "ntdll")]
unsafe extern "system" {
    pub fn NtSetInformationFile(
        FileHandle: HANDLE,
        IoStatusBlock: *mut IO_STATUS_BLOCK,
        FileInformation: LPVOID,
        Length: u32,
        FileInformationClass: u32,
    ) -> i32;

    pub fn RtlNtStatusToDosError(Status: i32) -> u32;

    pub fn NtQueryInformationFile(
        FileHandle: HANDLE,
        IoStatusBlock: *mut IO_STATUS_BLOCK,
        FileInformation: LPVOID,
        Length: u32,
        FileInformationClass: u32,
    ) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    pub fn OpenProcessToken(
        ProcessHandle: HANDLE,
        DesiredAccess: u32,
        TokenHandle: *mut HANDLE,
    ) -> BOOL;
    pub fn GetTokenInformation(
        TokenHandle: HANDLE,
        TokenInformationClass: u32,
        TokenInformation: LPVOID,
        TokenInformationLength: u32,
        ReturnLength: *mut u32,
    ) -> BOOL;
    pub fn AdjustTokenPrivileges(
        TokenHandle: HANDLE,
        DisableAllPrivileges: BOOL,
        NewState: *const TOKEN_PRIVILEGES,
        BufferLength: u32,
        PreviousState: *mut TOKEN_PRIVILEGES,
        ReturnLength: *mut u32,
    ) -> BOOL;
    pub fn LookupPrivilegeValueW(
        lpSystemName: *const u16,
        lpName: *const u16,
        lpLuid: *mut LUID,
    ) -> BOOL;
    pub fn GetSecurityInfo(
        handle: HANDLE,
        ObjectType: u32,
        SecurityInfo: u32,
        ppsidOwner: *mut LPVOID,
        ppsidGroup: *mut LPVOID,
        ppDacl: *mut LPVOID,
        ppSacl: *mut LPVOID,
        ppSecurityDescriptor: *mut LPVOID,
    ) -> u32;
    pub fn SetSecurityInfo(
        handle: HANDLE,
        ObjectType: u32,
        SecurityInfo: u32,
        psidOwner: LPVOID,
        psidGroup: LPVOID,
        pDacl: LPVOID,
        pSacl: LPVOID,
    ) -> u32;
}

pub const FILE_DEVICE_UNKNOWN: u32 = 0x22;
pub const FILE_ANY_ACCESS_X: u32 = 0;
