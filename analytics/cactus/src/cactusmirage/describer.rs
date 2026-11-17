//! The engine behind `cactusmirage`, replaceable by a fixture in tests.

use super::settings::Request;

/// Loads weights and describes packed RGB images.
pub trait Describer: Send {
    /// Loads the weights on the named engine to run with `threads`.
    /// Returns their SHA-256 and the threads the model runs with.
    fn start(
        &mut self,
        engine: &str,
        weights: &[u8],
        threads: u32,
    ) -> Result<(String, cactus::Threads), String>;
    /// Describes a packed RGB24 image.
    fn describe(
        &mut self,
        rgb: &[u8],
        width: u32,
        height: u32,
        request: &Request,
    ) -> Result<cactus::Description, String>;
}

/// A named Cactus engine, chosen at start.
#[derive(Default)]
pub struct Engine {
    name: String,
}

impl Engine {
    fn lock(&self) -> Result<cactus::Engine, String> {
        cactus::Engine::lock(&self.name).map_err(|err| err.to_string())
    }
}

impl Describer for Engine {
    fn start(
        &mut self,
        engine: &str,
        weights: &[u8],
        threads: u32,
    ) -> Result<(String, cactus::Threads), String> {
        engine.clone_into(&mut self.name);
        let mut engine = self.lock()?;
        let threads = engine
            .load_vision(weights, threads)
            .map_err(|err| err.to_string())?;
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
        self.lock()?
            .describe(image, request.as_engine())
            .map_err(|err| err.to_string())
    }
}
