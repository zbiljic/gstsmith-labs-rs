//! Opt-in tests with the real speaker model: `mise run //analytics/diarization:smoke`
//! (or set `DIARIZATION_WESPEAKER_MODEL` and run with `--ignored`).

use std::path::PathBuf;

use gstdiarization::embedder::{self, Embedder, Error};

/// Two tones and a little noise, generated the same way as the reference by
/// `scripts/reference.py`.
fn signal(n: usize) -> Vec<f32> {
    let mut state: u32 = 1;
    (0..n)
        .map(|i| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (f64::from(state >> 8) / 16_777_216.0 - 0.5) * 0.1;
            #[expect(clippy::cast_precision_loss, reason = "test signal")]
            let t = i as f64 / 16_000.0;
            let tau = std::f64::consts::TAU;
            #[expect(clippy::cast_possible_truncation, reason = "test signal")]
            let sample = (0.3 * (tau * 440.0 * t).sin()
                + 0.2 * (tau * 1250.0 * t + 0.5).sin()
                + noise) as f32;
            sample
        })
        .collect()
}

/// The model under test.
fn model() -> Option<PathBuf> {
    std::env::var_os("DIARIZATION_WESPEAKER_MODEL").map(PathBuf::from)
}

// `mise run //analytics/diarization:references` prints this: the Python
// pipeline (kaldi-native-fbank, mean subtraction, ONNX Runtime) on
// `signal(32000)`, normalized.
// onnxruntime 1.28.0, signal(32000), normalized
#[rustfmt::skip]
#[expect(clippy::unreadable_literal, reason = "printed by scripts/reference.py")]
const REFERENCE: [f32; 256] = [
    0.00362225, 0.0364535, 0.0165364, -0.0471413, 0.0434078, -0.0515985, 0.0194665, 0.0691821,
    0.0429685, -0.0223138, 0.0694643, 0.178123, -0.128684, 0.00187449, 0.0473325, 0.0405063,
    0.145919, 0.0530176, 0.0336123, 0.068822, 0.026489, -0.171536, 0.0453916, -0.00468105,
    0.0454271, 0.0326985, -0.100854, -0.127003, 0.000214355, 0.119588, 0.0511566, -0.0334785,
    0.0709472, -0.0246678, 0.0195603, 0.042822, 0.00424566, 0.0833488, -0.0226395, 0.0432693,
    0.00889381, 0.0464965, -0.0187406, -0.0273312, -0.0989159, 0.0542489, 0.0169918, -0.0404228,
    0.041533, 0.123024, 0.0714769, -0.0878274, 0.043775, 0.0283651, -0.0147262, 0.0433769,
    0.112751, 0.0345411, -0.0128579, -0.0383028, -0.0295412, -0.000133559, 0.0300587, -0.0140767,
    -0.0213748, 0.00527522, -0.0781757, 0.00388363, 0.0712238, -0.0169406, 0.0061054, -0.0381448,
    0.0704312, -0.00269797, 0.122426, -0.0647708, 0.0282229, -0.0688144, 0.0120884, 0.0801604,
    0.0914128, 0.017612, -0.0883734, -0.00216802, 0.0742195, 7.64083e-05, -0.0900891, -0.0716033,
    -0.0129183, 0.10197, -0.0543226, -0.0451818, -0.0417906, 0.0767777, -0.023134, 0.0528162,
    -0.0244666, 0.0944714, -0.0680344, 0.00932235, 0.0878737, -0.0580217, -0.0290035, -0.048836,
    -0.0429462, 0.0300671, -0.0279084, -0.0509898, -0.0298433, -0.019294, 0.0495321, -0.0264362,
    0.0227419, 0.0443478, -0.0292377, -0.105876, -0.105144, 0.0938312, -0.0625294, 0.104784,
    0.00988381, 0.0386005, 0.0978042, -0.113309, -0.105988, 0.0454465, -0.128946, 0.0346109,
    0.0187643, 0.0284016, -0.0967801, -0.0117669, 0.0152189, -0.034256, 0.0158693, 0.0320225,
    -0.00953402, -0.0170457, 0.055879, -0.0227654, 0.00630606, -0.0918221, 0.0556454, -0.0227397,
    -0.0502786, 0.0110413, -0.0644166, 0.000824149, 0.0760548, -0.00607044, 0.0565814, 0.0164114,
    -0.0349984, -0.0258169, -0.0122199, -0.0159789, 0.105072, -0.020125, 0.00904393, -0.0361358,
    -0.000613557, 0.0269332, 0.0248698, 0.0144054, -0.0508873, -0.0865922, -0.0522746, -0.00973773,
    -0.00523208, 0.0193587, 0.00998924, 0.0326938, 0.0505301, 0.0206762, -0.00904141, -0.0479629,
    -0.0955344, -0.0346998, -0.156825, 0.000543916, -0.172749, 0.0687475, -0.0525374, -0.0349395,
    -0.116967, -0.00464607, -0.0261035, 0.095401, 0.0752303, -0.0939901, 0.00388253, -0.0771585,
    0.00590233, -0.0426439, -0.0255713, 0.0161213, -0.047835, -0.0334545, -0.10649, -0.0514017,
    -0.0650921, 0.0911569, 0.0335805, 0.00662005, -0.00238514, -0.0944787, 0.0244883, -0.059896,
    0.0820676, -0.0404576, 0.00171894, 0.0970897, 0.0559881, 0.149704, -0.110058, 0.0129188,
    0.0207129, 0.0770568, -0.00614719, -0.0454296, -0.12017, -0.0192087, 0.0250615, 0.0837625,
    -0.0636753, -0.0849027, -0.0386076, -0.0376078, 0.0620421, -0.0603927, -0.0837456, 0.0125901,
    -0.0770696, -0.0191008, 0.102372, 0.0198278, -0.116329, -0.0382452, -0.0627422, 0.102381,
    -0.0450064, 0.0623394, 0.040856, -0.0587227, -0.0752727, -0.0620819, -0.0878535, -0.00638252,
    -0.0224785, 0.0268418, 0.0426564, -0.128221, 0.0960838, 0.0815545, 0.0514914, 0.0215708,
];

#[test]
#[ignore = "needs the speaker model; run `mise run //analytics/diarization:smoke`"]
fn wespeaker_matches_the_reference() {
    let path = model().expect("set DIARIZATION_WESPEAKER_MODEL");
    let mut embedder = Embedder::load(&path, None, 0).expect("a qualified model");
    assert_eq!(embedder.profile().name, "wespeaker-resnet34");
    assert!(embedder.is_qualified());
    assert_eq!(embedder.dim(), 256);

    let embedding = embedder.embed(&signal(32_000)).expect("embedded");
    let similarity = embedder::cosine(&embedding, &REFERENCE).expect("same length");
    assert!(similarity > 0.9999, "cosine to the reference {similarity}");

    let min = embedder.profile().min_samples;
    let short = embedder.embed(&signal(min - 1)).expect_err("too short");
    assert!(matches!(short, Error::TooShort { .. }) && short.is_segment_error());
    let mut bad = signal(min);
    if let Some(sample) = bad.get_mut(10) {
        *sample = f32::NAN;
    }
    assert!(matches!(
        embedder.embed(&bad),
        Err(Error::InvalidAudio { index: 10 })
    ));
    // Longer than the maximum: embedded in parts, still one unit vector
    // close to the embedding of the same sound.
    let long = embedder
        .embed(&signal(embedder.profile().max_samples + 16_000))
        .expect("embedded in parts");
    assert!((embedder::cosine(&long, &long).expect("same length") - 1.0).abs() < 1e-5);
    assert!(embedder::cosine(&long, &embedding).expect("same length") > 0.8);
}

#[test]
#[ignore = "needs the speaker model; run `mise run //analytics/diarization:smoke`"]
fn a_named_profile_is_checked_against_the_model() {
    let path = model().expect("set DIARIZATION_WESPEAKER_MODEL");
    let named = Embedder::load(&path, Some("wespeaker-resnet34"), 1).expect("named");
    assert_eq!(named.profile().name, "wespeaker-resnet34");
    assert!(matches!(
        Embedder::load(&path, Some("nope"), 0),
        Err(Error::UnknownProfile(_))
    ));
    // A profile whose input the model lacks is refused at load.
    let mut wrong = named.profile().clone();
    wrong.input = "audio".to_owned();
    let err = Embedder::new(&path, wrong, 0).expect_err("no such input");
    assert!(err.to_string().contains("no input audio"), "{err}");
    let missing = Embedder::load(&path.with_extension("missing"), None, 0).expect_err("no file");
    assert!(matches!(missing, Error::Io { .. }));
}
