//! `cactusmirage` properties and the engine request they make.

use gst::glib;
use serde_json::{Map, Value, json};

/// Top-level sections of a Mirage response.
#[glib::flags(name = "GstCactusMirageSections")]
pub enum Sections {
    #[flags_value(name = "Scene attributes", nick = "scene")]
    SCENE = 1 << 0,
    #[flags_value(name = "Objects", nick = "objects")]
    OBJECTS = 1 << 1,
    #[flags_value(name = "Regions", nick = "regions")]
    REGIONS = 1 << 2,
    #[flags_value(name = "Recognized text", nick = "text")]
    TEXT = 1 << 3,
    #[flags_value(name = "Relations between objects", nick = "relations")]
    RELATIONS = 1 << 4,
    #[flags_value(name = "Reasoning", nick = "reasoning")]
    REASONING = 1 << 5,
    #[flags_value(name = "Events (always empty so far)", nick = "events")]
    EVENTS = 1 << 6,
}

/// Per-object and per-line details. `bbox` is always requested, because an
/// `ODMtd` cannot exist without a box.
#[glib::flags(name = "GstCactusMirageFields")]
pub enum Fields {
    #[flags_value(name = "Bounding box", nick = "bbox")]
    BBOX = 1 << 0,
    #[flags_value(name = "Score", nick = "score")]
    SCORE = 1 << 1,
    #[flags_value(name = "A point on the object", nick = "point")]
    POINT = 1 << 2,
    #[flags_value(name = "Mask", nick = "mask")]
    MASK = 1 << 3,
    #[flags_value(name = "Coarse position", nick = "position")]
    POSITION = 1 << 4,
    #[flags_value(name = "Attributes such as colour", nick = "description")]
    DESCRIPTION = 1 << 5,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstCactusMirageMaskFormat")]
pub enum MaskFormat {
    /// One polygon per part of the mask.
    #[default]
    #[enum_value(name = "One polygon per part", nick = "polygons")]
    Polygons,
    #[enum_value(name = "The largest part as one polygon", nick = "largest")]
    Largest,
    #[enum_value(name = "The whole mask as one polygon", nick = "merged")]
    Merged,
    /// No masks are requested, whatever `fields` says.
    #[enum_value(name = "No masks", nick = "none")]
    None,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstCactusMirageJsonOutput")]
pub enum JsonOutput {
    /// The frame record and one document per object.
    #[default]
    #[enum_value(name = "Frame record and per-object documents", nick = "full")]
    Full,
    /// The frame record only.
    #[enum_value(name = "Frame record only", nick = "frame")]
    Frame,
    #[enum_value(name = "No Cactus documents", nick = "none")]
    None,
}

pub const DEFAULT_MAX_FRAME_BYTES: u32 = 128 * 1024 * 1024;
pub const MAX_THREADS: u32 = 256;

/// Options that have their own property, which `extra-options` may not set.
const OWNED_KEYS: &[&str] = &[
    "sections",
    "fields",
    "conf",
    "text_conf",
    "max_objects",
    "max_text_lines",
    "max_tokens",
    "nms",
    "mask_format",
    "form",
    "language",
    "keywords",
];

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub model_file: Option<String>,
    pub latency: u64,
    pub sections: Sections,
    pub fields: Fields,
    pub score_threshold: f32,
    pub text_score_threshold: f32,
    pub max_objects: u32,
    pub max_text_lines: u32,
    pub max_tokens: u32,
    pub nms: bool,
    pub language: Option<String>,
    pub keywords: Option<String>,
    pub form: Option<String>,
    pub extra_options: Option<String>,
    pub mask_format: MaskFormat,
    pub max_analysis_size: u32,
    pub max_frame_bytes: u32,
    pub json_output: JsonOutput,
    pub threads: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            model_file: None,
            latency: 0,
            sections: Sections::SCENE
                | Sections::OBJECTS
                | Sections::TEXT
                | Sections::RELATIONS
                | Sections::REASONING,
            fields: Fields::BBOX | Fields::SCORE | Fields::DESCRIPTION | Fields::MASK,
            score_threshold: 0.0,
            text_score_threshold: 0.0,
            max_objects: 0,
            max_text_lines: 0,
            max_tokens: 0,
            nms: true,
            language: None,
            keywords: None,
            form: None,
            extra_options: None,
            mask_format: MaskFormat::Polygons,
            max_analysis_size: 0,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            json_output: JsonOutput::Full,
            threads: 0,
        }
    }
}

/// A checked engine request: options JSON, language and keywords.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub options: String,
    pub language: Option<String>,
    pub keywords: Option<String>,
}

impl Request {
    pub fn as_engine(&self) -> cactus::Request<'_> {
        cactus::Request {
            options: &self.options,
            language: self.language.as_deref(),
            keywords: self.keywords.as_deref(),
        }
    }
}

impl Sections {
    fn names(self) -> Vec<&'static str> {
        [
            (Self::SCENE, "scene"),
            (Self::OBJECTS, "objects"),
            (Self::REGIONS, "regions"),
            (Self::TEXT, "text"),
            (Self::RELATIONS, "relations"),
            (Self::REASONING, "reasoning"),
            (Self::EVENTS, "events"),
        ]
        .into_iter()
        .filter(|(flag, _)| self.contains(*flag))
        .map(|(_, name)| name)
        .collect()
    }
}

impl Fields {
    fn names(self) -> Vec<&'static str> {
        [
            (Self::BBOX, "bbox"),
            (Self::SCORE, "score"),
            (Self::POINT, "point"),
            (Self::MASK, "mask"),
            (Self::POSITION, "position"),
            (Self::DESCRIPTION, "description"),
        ]
        .into_iter()
        .filter(|(flag, _)| self.contains(*flag))
        .map(|(_, name)| name)
        .collect()
    }
}

fn json_object(name: &str, text: Option<&str>) -> Result<Option<Map<String, Value>>, String> {
    let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
        return Ok(None);
    };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(object)) => Ok(Some(object)),
        Ok(_) => Err(format!("{name} must be a JSON object")),
        Err(err) => Err(format!("{name} is not JSON: {err}")),
    }
}

impl Settings {
    /// Builds the engine request, or explains what is wrong with the
    /// properties. The engine checks names and ranges itself (see the dry
    /// run at start).
    pub fn request(&self) -> Result<Request, String> {
        let mut options = Map::new();
        if let Some(extra) = json_object("extra-options", self.extra_options.as_deref())? {
            if let Some(key) = extra.keys().find(|key| OWNED_KEYS.contains(&key.as_str())) {
                return Err(format!(
                    "extra-options cannot set {key}; use its own property"
                ));
            }
            options.extend(extra);
        }
        options.insert("sections".to_owned(), json!(self.sections.names()));
        let mut fields = self.fields | Fields::BBOX;
        if self.mask_format == MaskFormat::None {
            fields.remove(Fields::MASK);
        }
        options.insert("fields".to_owned(), json!(fields.names()));
        for (key, value) in [
            ("conf", self.score_threshold),
            ("text_conf", self.text_score_threshold),
        ] {
            if value > 0.0 {
                options.insert(key.to_owned(), cactus_record_f32(value));
            }
        }
        for (key, value) in [
            ("max_objects", self.max_objects),
            ("max_text_lines", self.max_text_lines),
            ("max_tokens", self.max_tokens),
        ] {
            if value > 0 {
                options.insert(key.to_owned(), json!(value));
            }
        }
        options.insert("nms".to_owned(), json!(self.nms));
        let mask_format = match self.mask_format {
            MaskFormat::Polygons | MaskFormat::None => "polygons",
            MaskFormat::Largest => "largest",
            MaskFormat::Merged => "merged",
        };
        options.insert("mask_format".to_owned(), json!(mask_format));
        if let Some(form) = json_object("form", self.form.as_deref())? {
            if form.is_empty() {
                return Err("form has no fields".to_owned());
            }
            options.insert("form".to_owned(), Value::Object(form));
        }
        let keywords = self.keywords.as_deref().map(|keywords| {
            keywords
                .split(',')
                .map(str::trim)
                .filter(|keyword| !keyword.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        });
        Ok(Request {
            options: Value::Object(options).to_string(),
            language: self
                .language
                .clone()
                .filter(|language| !language.is_empty()),
            keywords: keywords.filter(|keywords| !keywords.is_empty()),
        })
    }
}

/// An `f32` as JSON at `f32` precision.
fn cactus_record_f32(value: f32) -> Value {
    analytics_meta::record::f32_value(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_demo() {
        let request = Settings::default().request().expect("request");
        let options: Value = serde_json::from_str(&request.options).expect("JSON");
        assert_eq!(
            options,
            json!({
                "sections": ["scene", "objects", "text", "relations", "reasoning"],
                "fields": ["bbox", "score", "mask", "description"],
                "nms": true,
                "mask_format": "polygons",
            })
        );
        assert_eq!(request.language, None);
        assert_eq!(request.keywords, None);
    }

    #[test]
    fn properties_become_options() {
        let settings = Settings {
            sections: Sections::OBJECTS,
            fields: Fields::MASK,
            mask_format: MaskFormat::None,
            score_threshold: 0.3,
            max_objects: 3,
            nms: false,
            language: Some("de".to_owned()),
            keywords: Some(" cup, saucer ,,".to_owned()),
            form: Some(r#"{"person_present":{"enum":["yes","no"]}}"#.to_owned()),
            extra_options: Some(r#"{"mask_tolerance":2}"#.to_owned()),
            ..Settings::default()
        };
        let request = settings.request().expect("request");
        let options: Value = serde_json::from_str(&request.options).expect("JSON");
        assert_eq!(
            options,
            json!({
                "sections": ["objects"],
                "fields": ["bbox"],
                "conf": 0.3,
                "max_objects": 3,
                "nms": false,
                "mask_format": "polygons",
                "form": {"person_present": {"enum": ["yes", "no"]}},
                "mask_tolerance": 2,
            })
        );
        assert_eq!(request.language.as_deref(), Some("de"));
        assert_eq!(request.keywords.as_deref(), Some("cup\nsaucer"));
    }

    #[test]
    fn bad_json_and_owned_keys_are_refused() {
        let with = |form: Option<&str>, extra: Option<&str>| Settings {
            form: form.map(str::to_owned),
            extra_options: extra.map(str::to_owned),
            ..Settings::default()
        };
        for (settings, message) in [
            (with(Some("{"), None), "form is not JSON"),
            (with(Some("[]"), None), "form must be a JSON object"),
            (with(Some("{}"), None), "form has no fields"),
            (with(None, Some(r#"{"conf":0.5}"#)), "cannot set conf"),
            (
                with(None, Some(r#"{"mask_format":"bitmap"}"#)),
                "cannot set mask_format",
            ),
        ] {
            let error = settings.request().expect_err(message);
            assert!(error.contains(message), "{error}");
        }
    }
}
