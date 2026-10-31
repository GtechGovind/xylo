use super::*;
#[test]
fn stereo_48khz_is_mixed_resampled_and_duration_preserved() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("stereo.wav");
    let mut writer = hound::WavWriter::create(
        &input,
        hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .unwrap();
    for i in 0..48000 {
        let value =
            (0.25 * (2. * std::f32::consts::PI * 440. * i as f32 / 48000.).sin() * 32767.) as i16;
        writer.write_sample(value).unwrap();
        writer.write_sample(value).unwrap();
    }
    writer.finalize().unwrap();
    let info = normalize(
        std::fs::File::open(&input).unwrap(),
        &root.path().join("mono.wav"),
        120,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(info.frames, 16000);
    assert_eq!(info.sample_rate, 16000);
    assert_eq!(info.channels, 1);
    let mut wav = hound::WavReader::open(info.path).unwrap();
    let values: Vec<_> = wav.samples::<i16>().map(Result::unwrap).collect();
    let rms = (values.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / values.len() as f64)
        .sqrt()
        / 32767.;
    assert!((0.16..0.19).contains(&rms));
}
#[test]
fn converter_cancels_without_leaving_an_output() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("in.wav");
    write_mono(&input, &vec![0.; 32000], 16000, &CancellationToken::new()).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let output = root.path().join("out.wav");
    assert!(normalize(std::fs::File::open(&input).unwrap(), &output, 120, &cancel).is_err());
    assert!(!output.exists());
}
#[test]
fn resampling_rejects_alias_energy_above_target_nyquist() {
    let samples: Vec<_> = (0..48000)
        .map(|i| (2. * std::f32::consts::PI * 12000. * i as f32 / 48000.).sin())
        .collect();
    let output = resample(&samples, 48000, 16000, &CancellationToken::new()).unwrap();
    let rms = (output[100..output.len() - 100]
        .iter()
        .map(|v| v.powi(2))
        .sum::<f32>()
        / (output.len() - 200) as f32)
        .sqrt();
    assert!(rms < 0.01, "alias RMS {rms}");
}
