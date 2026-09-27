use epscan::capabilities::{Holder, V700_FAMILY, V800_FAMILY, discovery_name, interpreter_scanner};
use epscan::transport::Transport;
use epscan::{Backend, Device, Session};
use std::time::Duration;

#[test]
fn mounted_slide_grid_contains_both_orientations_with_independent_frames() {
    let layout = V800_FAMILY.holder(Holder::V800Slides).unwrap();
    assert_eq!(layout.frames_mm.len(), 12);
    assert_eq!(layout.default_film_type, Some("positive"));
    assert!(layout.strip_groups.is_empty());
    for (index, rect) in layout.frames_mm.iter().enumerate() {
        assert_eq!(&rect[2..], &[36.0, 36.0]);
        assert!(rect[0] >= 0.0 && rect[0] + rect[2] <= 149.86);
        assert!(rect[1] >= 0.0 && rect[1] + rect[3] <= 246.38);
        if index % 3 > 0 {
            assert!(rect[0] > layout.frames_mm[index - 1][0]);
        }
    }
    assert!(layout.frame_rect(13, 0.0).is_err());
    assert!(V700_FAMILY.holder(Holder::V800Slides).is_err());
}

#[test]
fn v700_policy_is_provisional_and_does_not_inherit_holders_ir_or_warmup_retry() {
    assert!(V700_FAMILY.support_status.contains("provisional"));
    assert_eq!(V700_FAMILY.pid, 0x012c);
    assert!(V700_FAMILY.holders.is_empty());
    assert!(V700_FAMILY.infrared_mode.is_none());
    const { assert!(!V700_FAMILY.transfer.allow_start_warmup_recovery) };
    assert!(
        V700_FAMILY
            .sources
            .iter()
            .all(|source| source.max_y_dpi == 9600)
    );
    assert!(
        discovery_name(0x04b8, 0x012c)
            .unwrap()
            .contains("V700/V750")
    );
}

struct NoCommands;
impl Transport for NoCommands {
    fn read(&mut self, _: usize, _: Duration) -> epscan::Result<Vec<u8>> {
        panic!("Unsupported scanners must not receive protocol commands")
    }
    fn write(&mut self, _: &[u8], _: Duration) -> epscan::Result<usize> {
        panic!("Unsupported scanners must not receive protocol commands")
    }
}

#[test]
fn interpreter_devices_are_recognized_but_rejected_before_protocol_io() {
    for pid in [0x0130, 0x013b, 0x013a] {
        assert!(interpreter_scanner(0x04b8, pid).is_some());
        let device = Device {
            location: "test".into(),
            name: discovery_name(0x04b8, pid).unwrap().into(),
            vid: 0x04b8,
            pid,
            backend: Backend::Nusb,
        };
        let error = Session::with_transport(device, Box::new(NoCommands), Duration::from_secs(1))
            .err()
            .unwrap();
        assert!(error.to_string().contains("interpreter"));
    }
    assert!(discovery_name(0x1234, 0x0130).is_none());
}
