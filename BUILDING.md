# Hướng dẫn build skopeo (Windows / Linux RHEL)

Binary kết quả nhỏ (~2.2–2.6 MB) và các binary Linux là **fully static** (không
phụ thuộc glibc/OpenSSL) — build 1 lần chạy được trên mọi distro.

## 1. Tổng quan — chạy file nào

| Bạn đang ở | Chạy | Output |
|---|---|---|
| Windows (cmd) | `build.bat` | `dist\` |
| Windows (PowerShell) | `.\build.ps1` | `dist\` |
| Windows (Git Bash) | `./build.sh` | `dist/` |
| Linux / macOS | `./build.sh` | `dist/` |
| CI (GitHub) | push tag `v*` | tự build matrix, attach vào Release |

Mặc định build 3 target:

| Target | Kiểu | Kích thước thực tế |
|---|---|---|
| `x86_64-pc-windows-msvc` | Windows amd64 | 2.20 MB |
| `x86_64-unknown-linux-musl` | Linux amd64, static | 2.58 MB |
| `aarch64-unknown-linux-musl` | Linux arm64, static | 2.20 MB |

Chỉ định target khác: `build.bat x86_64-unknown-linux-musl` hoặc
`./build.sh aarch64-unknown-linux-musl` (xem bảng mục 4).

Kết quả đã verify bằng `file`:

```
skopeo-x86_64-unknown-linux-musl:  ELF 64-bit LSB executable, x86-64, statically linked, stripped
skopeo-aarch64-unknown-linux-musl: ELF 64-bit LSB executable, ARM aarch64, statically linked, stripped
```

---

## 2. Build trên Windows

### Bước 2.1 — Cài Rust (rustup) + MSVC Build Tools

1. Tải **rustup-init.exe** (chọn *64-bit MSVC*) tại <https://rustup.rs> và chạy,
   chọn *"Install with default host triplets"* (x86_64-pc-windows-msvc).
2. MSVC Build Tools (bắt buộc — Rust trên Windows dùng linker `link.exe`):
   - Tải <https://visualstudio.microsoft.com/visual-cpp-build-tools/> và chọn
     workload **Desktop development with C++**, hoặc:

   ```bat
   winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
   ```

3. Kiểm tra:

   ```bat
   rustc --version    REM ≥ 1.82
   cargo --version
   ```

### Bước 2.2 — Cài công cụ cross-build cho Linux target

Chỉ cần khi build `*-linux-*`. Binary Windows target không cần bước này.

```bat
cargo install cargo-zigbuild
pip install ziglang
```

- cargo-zigbuild: <https://github.com/rust-cross/cargo-zigbuild>
- zig (chọn 1 trong 3 cách):
  - `pip install ziglang` (đơn giản nhất, cargo-zigbuild tự nhận diện), hoặc
  - `winget install zig.zig`, hoặc
  - tải trực tiếp <https://ziglang.org/download/> và thêm vào `PATH`.
- Cặp phiên bản đã verify trên máy build: **cargo-zigbuild 0.24.0 + zig 0.14.1**.

### Bước 2.3 — Build

```bat
cd Rust-Base
build.bat
```

Script tự `rustup target add` cho từng target. Kết quả trong `dist\`:

```
dist\skopeo-x86_64-pc-windows-msvc.exe
dist\skopeo-x86_64-unknown-linux-musl
dist\skopeo-aarch64-unknown-linux-musl
```

---

## 3. Build trên Linux (RHEL 8/9 — RHEL/Rocky/Alma/CentOS Stream)

> ⚠️ Không dùng `dnf install rust` — phiên bản Rust trong repo RHEL quá cũ
> (project cần ≥ 1.82). Dùng rustup như dưới.

### Bước 3.1 — Công cụ hệ thống

```bash
sudo dnf install -y gcc gcc-c++ curl git
```

### Bước 3.2 — Rust qua rustup

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustc --version
```

### Bước 3.3 — cargo-zigbuild + zig

```bash
sudo dnf install -y python3-pip
cargo install cargo-zigbuild
pip3 install --user ziglang
export PATH="$HOME/.local/bin:$PATH"
```

- ziglang trên PyPI: <https://pypi.org/project/ziglang/>
- Hoặc tải zig tarball (<https://ziglang.org/download/>) rồi `export PATH` tới
  thư mục chứa `zig`.
- cargo-zigbuild tự `rustup target add` được làm bởi `build.sh`; nếu thiếu:

```bash
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
```

### Bước 3.4 — Build

```bash
cd Rust-Base
./build.sh
```

Kết quả trong `dist/`. Binary là static musl — **không cần cài glibc/musl-dev
gì thêm**, copy sang máy khác (kể cả không có internet) chạy được ngay.

Build trên máy amd64 vẫn ra được binary arm64 (và ngược lại) — cross-compile
qua zig, không cần qemu hay container.

---

## 4. Bảng target đầy đủ

| Target | Chạy ở đâu | Công cụ |
|---|---|---|
| `x86_64-pc-windows-msvc` | Windows | cargo (native) |
| `x86_64-unknown-linux-musl` | Windows/Linux/macOS | cargo-zigbuild + zig |
| `aarch64-unknown-linux-musl` | Windows/Linux/macOS | cargo-zigbuild + zig |
| `armv7-unknown-linux-musleabihf` | Windows/Linux/macOS | cargo-zigbuild + zig |
| `riscv64gc-unknown-linux-musl` | Windows/Linux/macOS | cargo-zigbuild + zig |
| `x86_64-apple-darwin` / `aarch64-apple-darwin` | chỉ trên macOS | cargo (native) |
| `aarch64-pc-windows-msvc` | bất kỳ | cargo-xwin (`cargo install cargo-xwin`) |

---

## 5. Kiểm tra kết quả

```bash
file dist/*            # ELF: "statically linked, stripped"
dist\skopeo-x86_64-pc-windows-msvc.exe --version
./dist/skopeo-x86_64-unknown-linux-musl --version   # trên máy Linux amd64
```

Unit + integration tests: `cargo test` (không cần network).

---

## 6. CI tự động

`.github/workflows/release.yml`: push tag `v*` lên GitHub → workflow build toàn
bộ matrix (4 Linux musl + 2 macOS + 2 Windows) trên runner tương ứng và đính
kèm binary vào GitHub Release.

---

## 7. Troubleshooting

| Triệu chứng | Nguyên nhân / cách xử lý |
|---|---|
| `error: linker 'link.exe' not found` | Chưa cài MSVC Build Tools (mục 2.1, bước 2). |
| `couldn't find zig` khi `cargo zigbuild` | zig chưa trong PATH: `pip install ziglang`, hoặc cài zig bằng winget/tải tay. |
| zig version mismatch từ cargo-zigbuild | Dùng cặp đã verify: cargo-zigbuild 0.24.x + zig 0.14.x (bảng tương thích: <https://github.com/rust-cross/cargo-zigbuild>). |
| `rustc --version` cũ hơn 1.82 | Cài qua rustup, đừng dùng rust của distro (RHEL/Debian đều cũ). |
| `cargo install` chậm / thất bại do mạng | Đặt proxy: `set HTTPS_PROXY=http://proxy:port` (Windows) / `export HTTPS_PROXY=...` (Linux). |
| Binary Linux báo `cannot execute` trên RHEL | Kiểm tra đúng kiến trúc: `file dist/*` — binary amd64 chạy trên x86_64, arm64 cần máy ARM. |
| Muốn binary nhỏ hơn nữa | Đã bật `opt-level="z"`, fat LTO, `strip`, `panic="abort"` trong `Cargo.toml`. Có thể dùng thêm `upx --best dist/skopeo-*` (giảm ~40% nữa, trade-off: startup giải nén). |
