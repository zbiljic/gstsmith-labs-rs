//! The engine behind `cactusmirage`, replaceable by a fixture in tests.

use super::settings::Request;

/// Loads weights and describes packed RGB images.
pub trait Describer: Send {
    /// Sets threads, then loads the weights. Returns their SHA-256 and what
    /// happened to the thread setting.
    fn start(&mut self, weights: &[u8], threads: u32) -> Result<(String, cactus::Threads), String>;
    /// Describes a packed RGB24 image.
    fn describe(
        &mut self,
        rgb: &[u8],
        width: u32,
        height: u32,
        request: &Request,
    ) -> Result<cactus::Description, String>;
}

/// The process-wide Cactus engine.
pub struct Engine;

impl Describer for Engine {
    fn start(&mut self, weights: &[u8], threads: u32) -> Result<(String, cactus::Threads), String> {
        let mut engine = cactus::Engine::lock();
        let threads = engine.set_threads(threads);
        engine.load_vision(weights).map_err(|err| err.to_string())?;
        Ok((cactus::sha256_hex(weights), threads))
    }

    fn describe(
        &mut self,
        rgb: &[u8],
        width: u32,
        height: u32,
        request: &Request,
    ) -> Result<cactus::Description, String> {
        let image = cactus::PackedRgb::new(rgb, width, height).map_err(|err| err.to_string())?;
        cactus::Engine::lock()
            .describe(image, request.as_engine())
            .map_err(|err| err.to_string())
    }
}
