use super::*;
#[test]
fn playback_decodes_and_mixes_without_opening_a_device() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("fixture.wav");
    let samples: Vec<_> = (0..16000)
        .map(|i| (2. * std::f32::consts::PI * 440. * i as f32 / 16000.).sin() * 0.2)
        .collect();
    super::super::convert::write_mono(&path, &samples, 16000, &CancellationToken::new()).unwrap();
    let decoder = Decoder::try_from(std::fs::File::open(path).unwrap()).unwrap();
    let (mixer, mut output) = rodio::mixer::mixer(2.try_into().unwrap(), 48000.try_into().unwrap());
    let player = rodio::Player::connect_new(&mixer);
    player.append(decoder);
    let rendered: Vec<_> = output.by_ref().take(4800).collect();
    assert_eq!(rendered.len(), 4800);
    assert!(rendered.iter().any(|sample| sample.abs() > 0.1));
    player.stop();
    let tail: Vec<_> = output.by_ref().take(24000).collect();
    assert!(
        tail[tail.len() - 4096..]
            .iter()
            .all(|sample| sample.abs() < 0.0001)
    );
}
