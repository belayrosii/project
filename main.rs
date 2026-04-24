mod audio;
mod model;

use audio::{ProductionAudioEngine, WasapiConfig};
use model::{Model, send_text_to_window};

use std::thread;
use std::time::{Duration, Instant};

fn main() {
    println!("[MAIN] Старт голосового ассистента...");

    let mut engine = ProductionAudioEngine::new("default");

    let cfg = WasapiConfig {
        sample_rate: 48000,
        channels: 2,
        buffer_duration_ms: 10,
    };

    engine.start(cfg, true).expect("ENGINE START FAILED");

    let model = Model::new().expect("MODEL INIT FAILED");

    println!("[MAIN] Готов. Откройте БЛОКНОТ и говорите...");
    println!("[MAIN] После каждой фразы будет пауза 10-30 сек на распознавание.");

    // Буфер для накопления речи
    let mut speech_buffer: Vec<f32> = Vec::with_capacity(48000); // 3 сек @ 16kHz
    let mut silence_frames: usize = 0;
    let mut speech_frames: usize = 0;

    // Параметры
    const MAX_SILENCE_FRAMES: usize = 25;   // ~250мс тишины = конец фразы
    const MIN_SPEECH_FRAMES: usize = 30;    // минимум 300мс речи
    const MAX_BUFFER_FRAMES: usize = 200;   // ~2 сек максимум (medium на CPU)

    let mut buf = vec![0.0f32; 4096];
    let mut last_status = Instant::now();
    let mut is_processing = false;

    loop {
        let (read, is_speech) = engine.read(&mut buf);

        if read == 0 {
            thread::sleep(Duration::from_millis(5));
            continue;
        }

        // Если идёт обработка предыдущей фразы — пропускаем новое аудио
        if is_processing {
            thread::sleep(Duration::from_millis(10));
            continue;
        }

        if is_speech {
            speech_buffer.extend_from_slice(&buf[..read]);
            speech_frames += 1;
            silence_frames = 0;
        } else {
            silence_frames += 1;
            // Небольшой хвост тишины
            if speech_frames > 0 && silence_frames <= 5 {
                speech_buffer.extend_from_slice(&buf[..read]);
            }
        }

        // Проверяем условия отправки
        let end_of_phrase = silence_frames >= MAX_SILENCE_FRAMES 
            && speech_frames >= MIN_SPEECH_FRAMES;
        let buffer_full = speech_frames >= MAX_BUFFER_FRAMES;

        if (end_of_phrase || buffer_full) && speech_frames > 0 {
            let samples = speech_buffer.len();
            let seconds = samples as f32 / 16000.0;

            println!("[MAIN] Отправляю {} сэмплов ({:.1} сек) на распознавание...", 
                samples, seconds);

            is_processing = true;

            // 🔥 Распознаём здесь, в главном потоке.
            // Medium на CPU: 2 сек = ~10-20 сек обработки.
            // Это нормально, просто подожди.
            let start = Instant::now();

            match model.transcribe(&speech_buffer) {
                Ok(Some(text)) => {
                    let elapsed = start.elapsed().as_secs_f32();
                    println!("[MAIN] ✅ Распознано за {:.1} сек: {}", elapsed, text);
                    send_text_to_window(&text);
                }
                Ok(None) => {
                    let elapsed = start.elapsed().as_secs_f32();
                    println!("[MAIN] ⚠️ Ничего не распознано (за {:.1} сек)", elapsed);
                }
                Err(e) => {
                    let elapsed = start.elapsed().as_secs_f32();
                    eprintln!("[MAIN] ❌ Ошибка за {:.1} сек: {:?}", elapsed, e);
                }
            }

            // Очищаем буфер
            speech_buffer.clear();
            speech_frames = 0;
            silence_frames = 0;
            is_processing = false;

            println!("[MAIN] Готов к следующей фразе.");
        }

        // Статус каждые 3 секунды
        if last_status.elapsed().as_secs() >= 3 {
            if speech_frames > 0 {
                println!("[MAIN] Накоплено: {} сэмплов ({:.1} сек), речь: {} фреймов", 
                    speech_buffer.len(),
                    speech_buffer.len() as f32 / 16000.0,
                    speech_frames
                );
            }
            last_status = Instant::now();
        }
    }
}
