use anyhow::{anyhow, Result};
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

type TessBaseAPI = libc::c_void;

const PSM_SPARSE_TEXT: libc::c_int = 11;

struct TessLib {
    create: unsafe extern "C" fn() -> *mut TessBaseAPI,
    delete: unsafe extern "C" fn(*mut TessBaseAPI),
    init3: unsafe extern "C" fn(
        *mut TessBaseAPI,
        *const libc::c_char,
        *const libc::c_char,
    ) -> libc::c_int,
    set_page_seg_mode: unsafe extern "C" fn(*mut TessBaseAPI, libc::c_int),
    set_variable: unsafe extern "C" fn(
        *mut TessBaseAPI,
        *const libc::c_char,
        *const libc::c_char,
    ) -> libc::c_int,
    set_image: unsafe extern "C" fn(
        *mut TessBaseAPI,
        *const libc::c_uchar,
        libc::c_int,
        libc::c_int,
        libc::c_int,
        libc::c_int,
    ),
    set_source_resolution: unsafe extern "C" fn(*mut TessBaseAPI, libc::c_int),
    recognize: unsafe extern "C" fn(*mut TessBaseAPI, *mut libc::c_void) -> libc::c_int,
    get_tsv_text: unsafe extern "C" fn(*mut TessBaseAPI, libc::c_int) -> *mut libc::c_char,
    delete_text: unsafe extern "C" fn(*const libc::c_char),
    clear: unsafe extern "C" fn(*mut TessBaseAPI),
}

unsafe fn load_sym<T>(handle: *mut libc::c_void, name: &[u8]) -> Option<T> {
    let ptr = libc::dlsym(handle, name.as_ptr() as *const libc::c_char);
    if ptr.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&ptr))
    }
}

impl TessLib {
    fn load() -> Option<Self> {
        // Derleme zamanı bağımlılığı olmadan tesseract kurulu olmayan sistemlerde de ikilinin açılabilmesi için.
        // Tesseract'ın OpenMP'si motor başına çekirdek sayısı kadar iş parçacığı açar; paralel bantlarla
        // birlikte aşırı abonelik oluşur (ölçüm: tam pencere 1,1 sn → 0,7 sn). libgomp bu değişkeni
        // yüklenirken okur, bu yüzden dlopen'dan önce ayarlanır (kullanıcı ayarladıysa dokunulmaz).
        if std::env::var_os("OMP_THREAD_LIMIT").is_none() {
            std::env::set_var("OMP_THREAD_LIMIT", "1");
        }
        unsafe {
            let flags = libc::RTLD_LAZY | libc::RTLD_LOCAL;
            let mut handle =
                libc::dlopen(b"libtesseract.so.5\0".as_ptr() as *const libc::c_char, flags);
            if handle.is_null() {
                handle = libc::dlopen(b"libtesseract.so\0".as_ptr() as *const libc::c_char, flags);
            }
            if handle.is_null() {
                return None;
            }

            let lib = Self {
                create: load_sym(handle, b"TessBaseAPICreate\0")?,
                delete: load_sym(handle, b"TessBaseAPIDelete\0")?,
                init3: load_sym(handle, b"TessBaseAPIInit3\0")?,
                set_page_seg_mode: load_sym(handle, b"TessBaseAPISetPageSegMode\0")?,
                set_variable: load_sym(handle, b"TessBaseAPISetVariable\0")?,
                set_image: load_sym(handle, b"TessBaseAPISetImage\0")?,
                set_source_resolution: load_sym(handle, b"TessBaseAPISetSourceResolution\0")?,
                recognize: load_sym(handle, b"TessBaseAPIRecognize\0")?,
                get_tsv_text: load_sym(handle, b"TessBaseAPIGetTsvText\0")?,
                delete_text: load_sym(handle, b"TessDeleteText\0")?,
                clear: load_sym(handle, b"TessBaseAPIClear\0")?,
            };
            Some(lib)
        }
    }
}

fn tess_lib() -> Option<&'static TessLib> {
    static LIB: OnceLock<Option<TessLib>> = OnceLock::new();
    LIB.get_or_init(TessLib::load).as_ref()
}

pub struct Engine {
    handle: *mut TessBaseAPI,
    lib: &'static TessLib,
    key: (PathBuf, String),
}

// Ham işaretçi yüzünden Engine varsayılan olarak !Sync kalır; yalnız iş parçacıkları arası sahiplik devrine izin verilir.
unsafe impl Send for Engine {}

impl Engine {
    fn new(datapath: &Path, lang: &str) -> Option<Self> {
        let lib = tess_lib()?;
        let dp_str = datapath.to_str()?;
        let c_dp = CString::new(dp_str).ok()?;
        let c_lang = CString::new(lang).ok()?;

        unsafe {
            let handle = (lib.create)();
            if handle.is_null() {
                return None;
            }
            if (lib.init3)(handle, c_dp.as_ptr(), c_lang.as_ptr()) != 0 {
                (lib.delete)(handle);
                return None;
            }
            (lib.set_page_seg_mode)(handle, PSM_SPARSE_TEXT);
            let _ = (lib.set_variable)(
                handle,
                b"user_defined_dpi\0".as_ptr() as *const libc::c_char,
                b"96\0".as_ptr() as *const libc::c_char,
            );
            Some(Self {
                handle,
                lib,
                key: (datapath.to_path_buf(), lang.to_string()),
            })
        }
    }

    pub fn tsv(&mut self, rgb: &[u8], w: u32, h: u32) -> Result<String> {
        if w == 0 || h == 0 {
            return Err(anyhow!("boş görüntü OCR edilemez"));
        }
        let expected = (w as usize)
            .checked_mul(h as usize)
            .and_then(|px| px.checked_mul(3))
            .ok_or_else(|| anyhow!("görüntü boyutu taşması"))?;
        if rgb.len() < expected {
            return Err(anyhow!("RGB tamponu yetersiz: {} < {}", rgb.len(), expected));
        }
        if self.handle.is_null() {
            return Err(anyhow!("tesseract motor işaretçisi null"));
        }

        let w_i = libc::c_int::try_from(w).map_err(|_| anyhow!("genişlik sınırı aşıldı"))?;
        let h_i = libc::c_int::try_from(h).map_err(|_| anyhow!("yükseklik sınırı aşıldı"))?;
        let bpl = w_i
            .checked_mul(3)
            .ok_or_else(|| anyhow!("satır bayt sayısı taşması"))?;

        unsafe {
            (self.lib.set_image)(self.handle, rgb.as_ptr(), w_i, h_i, 3, bpl);
            (self.lib.set_source_resolution)(self.handle, 96);
            let rc = (self.lib.recognize)(self.handle, std::ptr::null_mut());
            if rc != 0 {
                (self.lib.clear)(self.handle);
                return Err(anyhow!("TessBaseAPIRecognize başarısız: {}", rc));
            }
            let text_ptr = (self.lib.get_tsv_text)(self.handle, 0);
            if text_ptr.is_null() {
                (self.lib.clear)(self.handle);
                return Err(anyhow!("TessBaseAPIGetTsvText null döndü"));
            }
            let copied = CStr::from_ptr(text_ptr).to_owned();
            (self.lib.delete_text)(text_ptr);
            (self.lib.clear)(self.handle);
            Ok(copied.to_string_lossy().into_owned())
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                (self.lib.delete)(self.handle);
            }
            self.handle = std::ptr::null_mut();
        }
    }
}

fn engine_pool() -> &'static Mutex<Vec<Engine>> {
    static POOL: OnceLock<Mutex<Vec<Engine>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(Vec::new()))
}

/// Havuzdaki motorları kapatır. Kısa ömürlü CLI süreçlerinde çıkışta çağrılır: kapatılmayan motorlar
/// tesseract'ın statik önbelleğinde "LEAK" uyarıları basıyordu.
pub fn shutdown() {
    if let Ok(mut pool) = engine_pool().lock() {
        pool.clear();
    }
}

pub fn with_engine<R>(
    datapath: &Path,
    lang: &str,
    f: impl FnOnce(&mut Engine) -> R,
) -> Option<R> {
    tess_lib()?;

    // Havuz kilidi yalnız motor ödünç alınırken/iade edilirken tutulur ki paralel bantlar birbirini bloklamasın.
    let popped = {
        let mut pool = engine_pool().lock().ok()?;
        let pos = pool
            .iter()
            .position(|e| e.key.0 == datapath && e.key.1 == lang);
        pos.map(|idx| pool.swap_remove(idx))
    };

    let mut engine = match popped {
        Some(eng) => eng,
        None => Engine::new(datapath, lang)?,
    };

    let result = f(&mut engine);

    if let Ok(mut pool) = engine_pool().lock() {
        pool.push(engine);
    }

    Some(result)
}
