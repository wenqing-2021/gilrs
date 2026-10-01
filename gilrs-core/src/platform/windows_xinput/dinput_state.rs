//! Platform-independent parts of the DirectInput wire format.

pub(super) const AXIS_BASE: u32 = 0x10000;
pub(super) const BUTTON_BASE: u32 = 0x20000;

// DirectInput reports clockwise hundredths of degrees; 0xffff in the low
// word means centered. Y is down, matching SDL's joystick convention.
pub(super) fn pov(value: u32) -> (i32, i32) {
    if value & 0xffff == 0xffff || value >= 36000 {
        return (0, 0);
    }
    match ((value + 2250) / 4500) % 8 {
        0 => (0, -1),
        1 => (1, -1),
        2 => (1, 0),
        3 => (1, 1),
        4 => (0, 1),
        5 => (-1, 1),
        6 => (-1, 0),
        _ => (-1, -1),
    }
}

pub(super) fn is_xinput_path(path: &[u16]) -> bool {
    let end = path.iter().position(|c| *c == 0).unwrap_or(path.len());
    path[..end].windows(3).any(|part| {
        (part[0] == b'I' as u16 || part[0] == b'i' as u16)
            && (part[1] == b'G' as u16 || part[1] == b'g' as u16)
            && part[2] == b'_' as u16
    })
}

// SDL's Windows DirectInput GUID: USB bus, vendor, product, version zero.
pub(super) fn mapping_uuid(vendor: u16, product: u16) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[0] = 3;
    bytes[4..6].copy_from_slice(&vendor.to_le_bytes());
    bytes[8..10].copy_from_slice(&product.to_le_bytes());
    bytes
}

pub(super) fn xinput_y(value: i16) -> i32 {
    // gilrs centers a signed 16-bit range at -1. Reflect about that center;
    // widen first, retaining the asymmetric endpoint. gilrs clamps normalized
    // values, so -32769 represents the reflected positive maximum exactly.
    -i32::from(value) - 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hats_cover_diagonals_neutral_and_invalid_data() {
        let expected = [
            (0, -1),
            (1, -1),
            (1, 0),
            (1, 1),
            (0, 1),
            (-1, 1),
            (-1, 0),
            (-1, -1),
        ];
        for (i, xy) in expected.into_iter().enumerate() {
            assert_eq!(pov(i as u32 * 4500), xy);
        }
        for neutral in [u32::MAX, 0xffff, 36000, 123456] {
            assert_eq!(pov(neutral), (0, 0));
        }
        assert_eq!(pov(35999), (0, -1));
    }

    #[test]
    fn deduplication_is_per_interface_not_vendor_product() {
        assert!(is_xinput_path(
            &r"\\?\hid#vid_045e&pid_028e&IG_00"
                .encode_utf16()
                .collect::<Vec<_>>()
        ));
        assert!(is_xinput_path(
            &"hid#ig_01".encode_utf16().collect::<Vec<_>>()
        ));
        assert!(!is_xinput_path(
            &"hid#vid_045e&pid_028e".encode_utf16().collect::<Vec<_>>()
        ));
        assert!(!is_xinput_path(&[0, 73, 71, 95]));
    }

    #[test]
    fn sdl_guid_and_code_domains_are_stable() {
        assert_eq!(
            mapping_uuid(0x054c, 0x05c4),
            [3, 0, 0, 0, 0x4c, 5, 0, 0, 0xc4, 5, 0, 0, 0, 0, 0, 0]
        );
        assert!(AXIS_BASE > 30);
        assert!(BUTTON_BASE > AXIS_BASE + 32);
    }

    #[test]
    fn xinput_y_reflection_preserves_normalized_direction() {
        fn normalized(value: i32) -> f32 {
            ((value + 32769) as f32 / 32768.0 - 1.0).clamp(-1.0, 1.0)
        }
        for value in i16::MIN..=i16::MAX {
            assert_eq!(-normalized(xinput_y(value)), normalized(value as i32));
        }
    }
}
