//! 兼容旧 MSVC CRT 的链接垫片。
//!
//! 本机只有 VS2015（v140）工具链；rusqlite 的 bundled sqlite3.c 在链接时需要
//! `__imp_memchr` / `__imp_strchr` / `__imp_strrchr`，而 v140 的 UCRT 导入库里没有
//! 这三个符号。这里用 Rust 实现同名数据符号（导入指针），行为与 CRT 一致。
//! 在装有现代 MSVC Build Tools 的机器上，本实现同样可用，不需要删改。

use std::ffi::{c_char, c_int, c_void};

unsafe extern "C" fn memchr_shim(s: *const c_void, c: c_int, n: usize) -> *const c_void {
    let bytes = s as *const u8;
    let needle = c as u8;
    let mut i = 0usize;
    while i < n {
        if *bytes.add(i) == needle {
            return bytes.add(i) as *const c_void;
        }
        i += 1;
    }
    std::ptr::null()
}

unsafe extern "C" fn strchr_shim(s: *const c_char, c: c_int) -> *const c_char {
    let bytes = s as *const u8;
    let needle = c as u8;
    let mut i = 0usize;
    loop {
        let byte = *bytes.add(i);
        if byte == needle {
            return bytes.add(i) as *const c_char;
        }
        if byte == 0 {
            return std::ptr::null();
        }
        i += 1;
    }
}

unsafe extern "C" fn strrchr_shim(s: *const c_char, c: c_int) -> *const c_char {
    let bytes = s as *const u8;
    let needle = c as u8;
    let mut last: *const c_char = std::ptr::null();
    let mut i = 0usize;
    loop {
        let byte = *bytes.add(i);
        if byte == needle {
            last = bytes.add(i) as *const c_char;
        }
        if byte == 0 {
            return last;
        }
        i += 1;
    }
}

#[no_mangle]
pub static __imp_memchr: unsafe extern "C" fn(*const c_void, c_int, usize) -> *const c_void =
    memchr_shim;
#[no_mangle]
pub static __imp_strchr: unsafe extern "C" fn(*const c_char, c_int) -> *const c_char = strchr_shim;
#[no_mangle]
pub static __imp_strrchr: unsafe extern "C" fn(*const c_char, c_int) -> *const c_char =
    strrchr_shim;
