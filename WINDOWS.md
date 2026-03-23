# Windows Build

This client now targets the native Windows `MSVC + vcpkg` path instead of the Arch-to-MinGW cross-build path.

## Prerequisites

Install these on the Windows machine:

1. Visual Studio Build Tools with the Desktop C++ workload
2. Rust with the `x86_64-pc-windows-msvc` target
3. `vcpkg`
4. PowerShell

## Dependencies

The repo includes a `vcpkg.json` manifest for the native Windows dependencies:

- `ffmpeg` with `avcodec`, `avformat`, and `swscale`
- `opus`

## Build

Set `VCPKG_ROOT` to your local `vcpkg` checkout, then run:

```powershell
cd client
.\scripts\build-windows-msvc.ps1 -Configuration release
```

If `vcpkg` has not been bootstrapped yet:

```powershell
cd client
.\scripts\build-windows-msvc.ps1 -Configuration release -BootstrapVcpkg
```

The script will:

1. Install the `vcpkg` dependencies from `vcpkg.json`
2. Add the Rust `x86_64-pc-windows-msvc` target
3. Build `st-client.exe`
4. Stage the executable and runtime DLLs into `client/dist/windows-x64/<configuration>/`

## Notes

- The script sets `VCPKGRS_DYNAMIC=1` so `ffmpeg-sys-next` uses the dynamic `vcpkg` libraries.
- The script sets `OPUS_DYNAMIC=1` and `LIBOPUS_LIB_DIR` so `audiopus_sys` links against the `vcpkg` Opus package instead of trying to build or discover a separate copy.
- If you prefer a fully static Windows build later, that is a separate setup and would require different `vcpkg` triplets and link settings.
