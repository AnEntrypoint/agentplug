use std::alloc::{alloc, dealloc, Layout};
use std::mem;

use serde_json::json;

#[link(wasm_import_module = "env")]
extern "C" {
    fn host_lightpanda_crawl(body_ptr: u32, body_len: u32) -> u64;
}

#[no_mangle]
pub extern "C" fn plugkit_alloc(len: u32) -> u32 {
    if len == 0 {
        return 0;
    }
    let layout = Layout::from_size_align(len as usize, mem::align_of::<u8>()).unwrap();
    let ptr = unsafe { alloc(layout) };
    if ptr.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    ptr as u32
}

#[no_mangle]
pub extern "C" fn plugkit_free(ptr: u32, len: u32) {
    if ptr == 0 || len == 0 {
        return;
    }
    let layout = Layout::from_size_align(len as usize, mem::align_of::<u8>()).unwrap();
    unsafe { dealloc(ptr as *mut u8, layout) };
}

fn read_str(ptr: u32, len: u32) -> String {
    if len == 0 {
        return String::new();
    }
    unsafe {
        String::from_utf8_lossy(std::slice::from_raw_parts(ptr as *const u8, len as usize))
            .into_owned()
    }
}

fn return_json(value: serde_json::Value) -> u64 {
    let bytes = value.to_string().into_bytes();
    let ptr = plugkit_alloc(bytes.len() as u32);
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len()) };
    (ptr as u64) | ((bytes.len() as u64) << 32)
}

#[no_mangle]
pub extern "C" fn plugin_call(verb_ptr: u32, verb_len: u32, body_ptr: u32, body_len: u32) -> u64 {
    match read_str(verb_ptr, verb_len).as_str() {
        "crawl" => unsafe { host_lightpanda_crawl(body_ptr, body_len) },
        "capabilities" => return_json(json!({
            "ok": true,
            "plugin": "lightpanda",
            "verbs": ["crawl", "capabilities"],
            "body": "plain text: optional first line engine=lightpanda, then url=<url> or a bare URL, wait=<ms>, eval=<js> steps",
        })),
        other => return_json(json!({"ok": false, "error": "unknown_verb", "verb": other})),
    }
}
