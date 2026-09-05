use std::io::Read;
use std::sync::{Arc, Mutex};

pub struct PipeDrain {
    buffer: Arc<Mutex<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl PipeDrain {
    pub fn join(mut self) -> Vec<u8> {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        std::mem::take(&mut *self.buffer.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn take_so_far(&self) -> Vec<u8> {
        std::mem::take(&mut *self.buffer.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn settle_within(&mut self, wait: std::time::Duration) -> Vec<u8> {
        let deadline = std::time::Instant::now() + wait;
        while let Some(reader) = self.reader.as_ref() {
            if reader.is_finished() {
                let _ = self.reader.take().map(|r| r.join());
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self.take_so_far()
    }
}

pub fn drain_pipe_on_its_own_thread<R: Read + Send + 'static>(pipe: Option<R>) -> PipeDrain {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let Some(mut pipe) = pipe else {
        return PipeDrain { buffer, reader: None };
    };
    let sink = buffer.clone();
    let reader = std::thread::spawn(move || {
        let mut chunk = [0u8; 64 * 1024];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&chunk[..n]),
            }
        }
    });
    PipeDrain { buffer, reader: Some(reader) }
}
