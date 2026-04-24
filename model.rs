use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::ptr::NonNull;

use enigo::{Enigo, Settings, Keyboard};

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug)]
pub enum ModelError {
    ModelNotFound(String),
    InvalidPath,
    InitFailed,
    StateInitFailed,
    InferenceFailed,
}

// ============================================================================
// FFI — подтверждено через dumpbin whisper.lib
// ============================================================================

#[repr(C)]
pub struct WhisperContext {
    _private: [u8; 0],
}

#[repr(C)]
pub struct WhisperState {
    _private: [u8; 0],
}

// 🔥 FIX: размер увеличен с 512 до 2048 байт.
// whisper_full_params в актуальной whisper.cpp значительно больше 512 байт.
// Неправильный размер = UB, повреждение стека, молчание модели.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WhisperFullParams {
    pub _data: [u8; 2048],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub enum WhisperSamplingStrategy {
    Greedy = 0,
    BeamSearch = 1,
}

extern "C" {
    // --- Инициализация ---
    fn whisper_init_from_file(path: *const c_char) -> *mut WhisperContext;
    fn whisper_free(ctx: *mut WhisperContext);

    fn whisper_init_state(ctx: *mut WhisperContext) -> *mut WhisperState;
    fn whisper_free_state(state: *mut WhisperState);

    // --- Параметры ---
    // Возвращает struct по значению (подтверждено C-заголовком и dumpbin)
    fn whisper_full_default_params(
        strategy: WhisperSamplingStrategy,
    ) -> WhisperFullParams;

    // --- Распознавание ---
    // params передаётся ПО ЗНАЧЕНИЮ (не по указателю!) — подтверждено whisper.h
    fn whisper_full_with_state(
        ctx: *mut WhisperContext,
        state: *mut WhisperState,
        params: WhisperFullParams,
        samples: *const f32,
        n_samples: c_int,
    ) -> c_int;

    // --- Результаты — ПОДТВЕРЖДЕНО: принимают ТОЛЬКО state, без ctx ---
    fn whisper_full_n_segments_from_state(
        state: *mut WhisperState,
    ) -> c_int;

    fn whisper_full_get_segment_text_from_state(
        state: *mut WhisperState,
        index: c_int,
    ) -> *const c_char;
}

// ============================================================================
// Model
// ============================================================================

pub struct Model {
    ctx: NonNull<WhisperContext>,
    state: NonNull<WhisperState>,
}

impl Model {
    pub fn new() -> Result<Self, ModelError> {
        let exe = std::env::current_exe().map_err(|_| ModelError::InvalidPath)?;
        let base = exe.parent().ok_or(ModelError::InvalidPath)?;

        let model_path = base.join("models").join("ggml-medium.bin");

        if !model_path.exists() {
            return Err(ModelError::ModelNotFound(
                model_path.to_string_lossy().into_owned(),
            ));
        }

        let c_path = CString::new(model_path.to_string_lossy().as_bytes())
            .map_err(|_| ModelError::InvalidPath)?;

        let ctx_ptr = unsafe { whisper_init_from_file(c_path.as_ptr()) };
        let ctx = NonNull::new(ctx_ptr).ok_or(ModelError::InitFailed)?;

        let state_ptr = unsafe { whisper_init_state(ctx.as_ptr()) };
        let state = NonNull::new(state_ptr).ok_or_else(|| {
            unsafe { whisper_free(ctx.as_ptr()) };
            ModelError::StateInitFailed
        })?;

        Ok(Self { ctx, state })
    }

    /// Распознаёт накопленный аудиобуфер.
    /// Возвращает текст или None если ничего не распознано.
    pub fn transcribe(&self, pcm: &[f32]) -> Result<Option<String>, ModelError> {
        if pcm.is_empty() {
            return Ok(None);
        }

        // Создаём дефолтные параметры (копия на стеке)
        let params = unsafe {
            whisper_full_default_params(WhisperSamplingStrategy::Greedy)
        };

        // Запускаем inference
        let code = unsafe {
            whisper_full_with_state(
                self.ctx.as_ptr(),
                self.state.as_ptr(),
                params,              // ← по значению!
                pcm.as_ptr(),
                pcm.len() as c_int,
            )
        };

        if code != 0 {
            eprintln!("[MODEL] whisper_full_with_state вернул ошибку: {}", code);
            return Err(ModelError::InferenceFailed);
        }

        // Получаем количество сегментов — ТОЛЬКО state, без ctx!
        let n = unsafe {
            whisper_full_n_segments_from_state(self.state.as_ptr())
        }.max(0);

        let mut result = String::new();

        for i in 0..n {
            // Получаем текст сегмента — ТОЛЬКО state + index, без ctx!
            let ptr = unsafe {
                whisper_full_get_segment_text_from_state(self.state.as_ptr(), i)
            };

            if ptr.is_null() {
                continue;
            }

            let text = unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .trim()
                .to_string();

            if !text.is_empty() {
                if !result.is_empty() {
                    result.push(' ');
                }
                result.push_str(&text);
            }
        }

        let final_text = result.trim().to_string();

        if final_text.is_empty() {
            Ok(None)
        } else {
            Ok(Some(final_text))
        }
    }
}

/// Отправляет текст в активное окно через enigo.
/// Вынесено отдельно от transcribe для чёткости и отладки.
pub fn send_text_to_window(text: &str) {
    if text.is_empty() {
        return;
    }

    match Enigo::new(&Settings::default()) {
        Ok(mut enigo) => {
            if let Err(e) = enigo.text(text) {
                eprintln!("[ENIGO] Ошибка ввода текста: {:?}", e);
            } else {
                println!("[ENIGO] Текст отправлен: {}", text);
            }
        }
        Err(e) => {
            eprintln!("[ENIGO] Не удалось создать Enigo: {:?}", e);
        }
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        unsafe {
            whisper_free_state(self.state.as_ptr());
            whisper_free(self.ctx.as_ptr());
        }
    }
}
