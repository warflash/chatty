//! Runtime binding to llama.cpp's prebuilt `llama.dll` / `libllama.dylib`
//! (pinned build in scripts/fetch-native.*; struct layouts below mirror its
//! `llama.h`).
//!
//! macOS: the dylibs carry versioned ggml names (`libggml.1.dylib`) and an
//! `@loader_path` rpath, so `dlopen` by full path binds them to their own
//! ggml with no further help.
//!
//! Windows: llama.cpp ships its own `ggml.dll` / `ggml-base.dll`, a different ggml
//! build than transcribe.cpp's DLLs of the same names, which are already
//! loaded. Windows would bind llama.dll's imports to those by module name.
//! Loading it inside an activation context built from `llama.manifest`
//! (a side-by-side assembly listing the DLLs in `llama\`) redirects the
//! imports to the right copies.

use std::ffi::{CString, c_char, c_void};
use std::path::Path;

#[cfg(windows)]
use windows::Win32::Foundation::{HANDLE, HMODULE};
#[cfg(windows)]
use windows::Win32::System::ApplicationInstallationAndServicing::{
    ACTCTXW, ActivateActCtx, CreateActCtxW, DeactivateActCtx,
};
#[cfg(windows)]
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetProcAddress, LoadLibraryW};
#[cfg(windows)]
use windows::core::{HSTRING, PCSTR, PCWSTR};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ModelParams {
    devices: *mut c_void,
    tensor_buft_overrides: *const c_void,
    pub n_gpu_layers: i32,
    split_mode: i32,
    load_mode: i32,
    lazy_mode: i32,
    main_gpu: i32,
    tensor_split: *const f32,
    progress_callback: *const c_void,
    progress_callback_user_data: *mut c_void,
    kv_overrides: *const c_void,
    vocab_only: bool,
    check_tensors: bool,
    use_extra_bufts: bool,
    no_host: bool,
    no_alloc: bool,
    load_mtp: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ContextParams {
    pub n_ctx: u32,
    pub n_batch: u32,
    pub n_ubatch: u32,
    n_seq_max: u32,
    n_rs_seq: u32,
    n_outputs_max: u32,
    n_outputs_max_per_seq: u32,
    pub n_threads: i32,
    pub n_threads_batch: i32,
    ctx_type: i32,
    rope_scaling_type: i32,
    pooling_type: i32,
    attention_type: i32,
    flash_attn_type: i32,
    rope_freq_base: f32,
    rope_freq_scale: f32,
    yarn_ext_factor: f32,
    yarn_attn_factor: f32,
    yarn_beta_fast: f32,
    yarn_beta_slow: f32,
    yarn_orig_ctx: u32,
    cb_eval: *const c_void,
    cb_eval_user_data: *mut c_void,
    type_k: i32,
    type_v: i32,
    moe_cache_size: usize,
    moe_cache_cpu_share: f32,
    abort_callback: *const c_void,
    abort_callback_data: *mut c_void,
    embeddings: bool,
    offload_kqv: bool,
    pub no_perf: bool,
    op_offload: bool,
    swa_full: bool,
    kv_unified: bool,
    samplers: *mut c_void,
    n_samplers: usize,
    ctx_other: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Batch {
    n_tokens: i32,
    token: *mut i32,
    embd: *mut f32,
    pos: *mut i32,
    n_seq_id: *mut i32,
    seq_id: *mut *mut i32,
    logits: *mut i8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ChainParams {
    no_perf: bool,
}

type Ptr = *mut c_void;

#[cfg(windows)]
const ACTCTX_FLAG_ASSEMBLY_DIRECTORY_VALID: u32 = 0x4;

#[allow(clippy::type_complexity)]
struct Api {
    backend_init: unsafe extern "C" fn(),
    load_all_from_path: unsafe extern "C" fn(*const c_char),
    log_set: unsafe extern "C" fn(*const c_void, Ptr),
    model_default_params: unsafe extern "C" fn() -> ModelParams,
    context_default_params: unsafe extern "C" fn() -> ContextParams,
    model_load_from_file: unsafe extern "C" fn(*const c_char, ModelParams) -> Ptr,
    model_free: unsafe extern "C" fn(Ptr),
    init_from_model: unsafe extern "C" fn(Ptr, ContextParams) -> Ptr,
    free: unsafe extern "C" fn(Ptr),
    model_get_vocab: unsafe extern "C" fn(Ptr) -> Ptr,
    tokenize: unsafe extern "C" fn(Ptr, *const c_char, i32, *mut i32, i32, bool, bool) -> i32,
    token_to_piece: unsafe extern "C" fn(Ptr, i32, *mut c_char, i32, i32, bool) -> i32,
    vocab_is_eog: unsafe extern "C" fn(Ptr, i32) -> bool,
    batch_get_one: unsafe extern "C" fn(*mut i32, i32) -> Batch,
    decode: unsafe extern "C" fn(Ptr, Batch) -> i32,
    get_memory: unsafe extern "C" fn(Ptr) -> Ptr,
    memory_clear: unsafe extern "C" fn(Ptr, bool),
    chain_init: unsafe extern "C" fn(ChainParams) -> Ptr,
    chain_add: unsafe extern "C" fn(Ptr, Ptr),
    init_greedy: unsafe extern "C" fn() -> Ptr,
    sample: unsafe extern "C" fn(Ptr, Ptr, i32) -> i32,
    sampler_free: unsafe extern "C" fn(Ptr),
}

#[cfg(windows)]
unsafe fn sym<T: Copy>(lib: HMODULE, name: &str) -> Result<T, String> {
    let c = CString::new(name).unwrap();
    let p = unsafe { GetProcAddress(lib, PCSTR(c.as_ptr() as *const u8)) }.ok_or_else(|| format!("llama.dll: no {name}"))?;
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

#[cfg(windows)]
fn module_path(m: HMODULE) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetModuleFileNameW(Some(m), &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

unsafe extern "C" fn quiet_log(_level: i32, _text: *const c_char, _user: Ptr) {}

static API: std::sync::OnceLock<Result<Api, String>> = std::sync::OnceLock::new();

/// Activation context for `dir\llama.manifest`; active until dropped.
#[cfg(windows)]
struct ActCtx(usize);

#[cfg(windows)]
impl ActCtx {
    fn activate(dir: &Path) -> Result<ActCtx, String> {
        let manifest = HSTRING::from(dir.join("llama.manifest").as_os_str());
        let assembly_dir = HSTRING::from(dir.as_os_str());
        let ctx = ACTCTXW {
            cbSize: std::mem::size_of::<ACTCTXW>() as u32,
            dwFlags: ACTCTX_FLAG_ASSEMBLY_DIRECTORY_VALID,
            lpSource: PCWSTR(manifest.as_ptr()),
            lpAssemblyDirectory: PCWSTR(assembly_dir.as_ptr()),
            ..Default::default()
        };
        let handle: HANDLE = unsafe { CreateActCtxW(&ctx) }.map_err(|e| format!("llama manifest: {e}"))?;
        let mut cookie = 0usize;
        unsafe { ActivateActCtx(Some(handle), &mut cookie) }.map_err(|e| format!("activate llama context: {e}"))?;
        // The context handle stays alive for the process (DLLs loaded in it
        // keep resolving through it).
        Ok(ActCtx(cookie))
    }
}

#[cfg(windows)]
impl Drop for ActCtx {
    fn drop(&mut self) {
        let _ = unsafe { DeactivateActCtx(0, self.0) };
    }
}

#[cfg(windows)]
fn backend_scope(dir: &Path) -> Result<ActCtx, String> {
    ActCtx::activate(dir)
}

#[cfg(target_os = "macos")]
fn backend_scope(_dir: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
unsafe fn sym<T: Copy>(lib: *mut c_void, name: &str) -> Result<T, String> {
    let c = CString::new(name).unwrap();
    let p = unsafe { libc::dlsym(lib, c.as_ptr()) };
    if p.is_null() {
        return Err(format!("{}: no {name}", crate::paths::LLAMA_LIB));
    }
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

#[cfg(target_os = "macos")]
fn dlopen(path: &Path) -> Result<*mut c_void, String> {
    let c = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    let lib = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if lib.is_null() {
        let err = unsafe { std::ffi::CStr::from_ptr(libc::dlerror()) }.to_string_lossy().into_owned();
        return Err(err);
    }
    Ok(lib)
}

#[cfg(target_os = "macos")]
fn load_api() -> Result<Api, String> {
    let dir = crate::paths::llama_dir();
    let path = dir.join(crate::paths::LLAMA_LIB);
    if !path.exists() {
        return Err(format!("{} not found in {}", crate::paths::LLAMA_LIB, dir.display()));
    }
    let lib = dlopen(&path)?;
    // Already loaded by libllama; this just returns its handle.
    let ggml = dlopen(&dir.join("libggml.1.dylib"))?;
    log::info!("llama.cpp loaded: {}", path.display());
    unsafe { finish_api(lib, ggml, &dir) }
}

#[cfg(windows)]
fn load_api() -> Result<Api, String> {
    let dir = crate::paths::llama_dir();
    if !dir.join("llama.dll").exists() {
        return Err(format!("llama.dll not found in {}", dir.display()));
    }
    let _ctx = ActCtx::activate(&dir)?;
    unsafe {
        let lib = LoadLibraryW(&HSTRING::from(dir.join("llama.dll").as_os_str())).map_err(|e| format!("llama.dll: {e}"))?;
        // Resolves inside the context, i.e. llama.cpp's own ggml.dll.
        let ggml = LoadLibraryW(&HSTRING::from("ggml.dll")).map_err(|e| format!("ggml.dll: {e}"))?;
        let ggml_path = module_path(ggml);
        log::info!("llama.cpp loaded: {} (ggml {})", module_path(lib), ggml_path);
        if !Path::new(&ggml_path).starts_with(&dir) {
            return Err(format!("llama.cpp bound to the wrong ggml: {ggml_path}"));
        }
        finish_api(lib, ggml, &dir)
    }
}

#[cfg(windows)]
type Lib = HMODULE;
#[cfg(target_os = "macos")]
type Lib = *mut c_void;

/// Resolves the entry points and initializes the backends.
unsafe fn finish_api(lib: Lib, ggml: Lib, dir: &Path) -> Result<Api, String> {
    unsafe {
        let api = Api {
            backend_init: sym(lib, "llama_backend_init")?,
            load_all_from_path: sym(ggml, "ggml_backend_load_all_from_path")?,
            log_set: sym(lib, "llama_log_set")?,
            model_default_params: sym(lib, "llama_model_default_params")?,
            context_default_params: sym(lib, "llama_context_default_params")?,
            model_load_from_file: sym(lib, "llama_model_load_from_file")?,
            model_free: sym(lib, "llama_model_free")?,
            init_from_model: sym(lib, "llama_init_from_model")?,
            free: sym(lib, "llama_free")?,
            model_get_vocab: sym(lib, "llama_model_get_vocab")?,
            tokenize: sym(lib, "llama_tokenize")?,
            token_to_piece: sym(lib, "llama_token_to_piece")?,
            vocab_is_eog: sym(lib, "llama_vocab_is_eog")?,
            batch_get_one: sym(lib, "llama_batch_get_one")?,
            decode: sym(lib, "llama_decode")?,
            get_memory: sym(lib, "llama_get_memory")?,
            memory_clear: sym(lib, "llama_memory_clear")?,
            chain_init: sym(lib, "llama_sampler_chain_init")?,
            chain_add: sym(lib, "llama_sampler_chain_add")?,
            init_greedy: sym(lib, "llama_sampler_init_greedy")?,
            sample: sym(lib, "llama_sampler_sample")?,
            sampler_free: sym(lib, "llama_sampler_free")?,
        };
        if std::env::var_os("CHATTY_LLAMA_LOG").is_none() {
            (api.log_set)(quiet_log as *const c_void, std::ptr::null_mut());
        }
        (api.backend_init)();
        let cdir = CString::new(dir.to_string_lossy().as_bytes()).unwrap();
        (api.load_all_from_path)(cdir.as_ptr());
        Ok(api)
    }
}

fn api() -> Result<&'static Api, String> {
    API.get_or_init(load_api).as_ref().map_err(Clone::clone)
}

pub struct Llm {
    api: &'static Api,
    model: Ptr,
    ctx: Ptr,
    vocab: Ptr,
    sampler: Ptr,
    n_ctx: u32,
}

// The handles are used from the engine thread only; moving them is fine.
unsafe impl Send for Llm {}

impl Llm {
    pub fn load(path: &Path, gpu_layers: i32, n_ctx: u32) -> Result<Llm, String> {
        let api = api()?;
        let dir = crate::paths::llama_dir();
        // Backends load lazily during model load; keep the context active.
        let _ctx = backend_scope(&dir)?;
        unsafe {
            let mut mp = (api.model_default_params)();
            mp.n_gpu_layers = gpu_layers;
            let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
            let model = (api.model_load_from_file)(cpath.as_ptr(), mp);
            if model.is_null() {
                return Err(format!("cannot load {}", path.display()));
            }
            let mut cp = (api.context_default_params)();
            cp.n_ctx = n_ctx;
            cp.n_batch = n_ctx;
            cp.n_ubatch = 512;
            cp.no_perf = true;
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get() as i32).min(8);
            cp.n_threads = threads;
            cp.n_threads_batch = threads;
            let ctx = (api.init_from_model)(model, cp);
            if ctx.is_null() {
                (api.model_free)(model);
                return Err("cannot create llama context".into());
            }
            let vocab = (api.model_get_vocab)(model);
            let sampler = (api.chain_init)(ChainParams { no_perf: true });
            (api.chain_add)(sampler, (api.init_greedy)());
            Ok(Llm { api, model, ctx, vocab, sampler, n_ctx })
        }
    }

    pub fn tokenize(&self, text: &str) -> Result<Vec<i32>, String> {
        let mut tokens = vec![0i32; text.len() + 16];
        let n = unsafe {
            (self.api.tokenize)(
                self.vocab,
                text.as_ptr() as *const c_char,
                text.len() as i32,
                tokens.as_mut_ptr(),
                tokens.len() as i32,
                false,
                true,
            )
        };
        if n < 0 {
            return Err("tokenize failed".into());
        }
        tokens.truncate(n as usize);
        Ok(tokens)
    }

    fn piece(&self, token: i32) -> Vec<u8> {
        let mut buf = vec![0u8; 64];
        let mut n = unsafe { (self.api.token_to_piece)(self.vocab, token, buf.as_mut_ptr() as *mut c_char, 64, 0, false) };
        if n < 0 {
            buf.resize((-n) as usize, 0);
            n = unsafe {
                (self.api.token_to_piece)(self.vocab, token, buf.as_mut_ptr() as *mut c_char, buf.len() as i32, 0, false)
            };
        }
        buf.truncate(n.max(0) as usize);
        buf
    }

    /// Greedy completion of `prompt`. Returns (text, prompt tokens, generated tokens).
    pub fn complete(&mut self, prompt: &str, max_tokens: usize) -> Result<(String, usize, usize), String> {
        let mut tokens = self.tokenize(prompt)?;
        let n_prompt = tokens.len();
        if n_prompt + max_tokens > self.n_ctx as usize {
            return Err(format!("prompt too long ({n_prompt} tokens)"));
        }
        unsafe {
            (self.api.memory_clear)((self.api.get_memory)(self.ctx), true);
            let batch = (self.api.batch_get_one)(tokens.as_mut_ptr(), tokens.len() as i32);
            if (self.api.decode)(self.ctx, batch) != 0 {
                return Err("decode failed".into());
            }
            let mut out = Vec::new();
            let mut generated = 0;
            while generated < max_tokens {
                let mut tok = (self.api.sample)(self.sampler, self.ctx, -1);
                if (self.api.vocab_is_eog)(self.vocab, tok) {
                    break;
                }
                out.extend(self.piece(tok));
                generated += 1;
                let batch = (self.api.batch_get_one)(&mut tok, 1);
                if (self.api.decode)(self.ctx, batch) != 0 {
                    return Err("decode failed".into());
                }
            }
            Ok((String::from_utf8_lossy(&out).into_owned(), n_prompt, generated))
        }
    }
}

impl Drop for Llm {
    fn drop(&mut self) {
        unsafe {
            (self.api.sampler_free)(self.sampler);
            (self.api.free)(self.ctx);
            (self.api.model_free)(self.model);
        }
    }
}
