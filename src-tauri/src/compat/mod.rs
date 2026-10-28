//! 平台/工具链兼容垫片。

#[cfg(all(target_os = "windows", target_env = "msvc"))]
mod msvc_crt;
