use std::mem::MaybeUninit;

fn enc(data: &[u8]) -> Vec<u8> {
    snap::raw::Encoder::new().compress_vec(data).unwrap()
}

fn check_roundtrip(data: &[u8]) {
    let compressed = enc(data);

    assert_eq!(snipsnap::decompress_len(&compressed).unwrap(), data.len());
    assert_eq!(snipsnap::decompress(&compressed).unwrap(), data);

    let mut initialized = vec![0xff; data.len() + 17];
    let decoded = snipsnap::decompress_into(&compressed, &mut initialized).unwrap();
    assert_eq!(decoded, data);

    let mut uninit = Vec::<MaybeUninit<u8>>::with_capacity(data.len() + 17);
    unsafe {
        uninit.set_len(data.len() + 17);
    }
    let decoded = snipsnap::decompress_into_uninit(&compressed, &mut uninit).unwrap();
    assert_eq!(decoded, data);

    let mut reused = Vec::with_capacity(data.len());
    let decoded = snipsnap::decompress_to_vec(&compressed, &mut reused).unwrap();
    assert_eq!(decoded, data);

    let mut snap_decoder = snap::raw::Decoder::new();
    assert_eq!(snap_decoder.decompress_vec(&compressed).unwrap(), data);
}

fn raw_header(mut len: usize, out: &mut Vec<u8>) {
    while len >= 0x80 {
        out.push((len as u8) | 0x80);
        len >>= 7;
    }
    out.push(len as u8);
}

fn raw_literal(data: &[u8], out: &mut Vec<u8>) {
    if data.len() <= 60 {
        out.push(((data.len() - 1) << 2) as u8);
    } else {
        out.push(60 << 2);
        out.push((data.len() - 1) as u8);
    }
    out.extend_from_slice(data);
}

fn raw_copy2(len: usize, offset: usize, out: &mut Vec<u8>) {
    out.push((((len - 1) << 2) | 2) as u8);
    out.extend_from_slice(&(offset as u16).to_le_bytes());
}

#[test]
fn roundtrip_snap_encoded_cases() {
    check_roundtrip(b"");
    check_roundtrip(b"a");
    check_roundtrip(b"hello snappy");
    check_roundtrip(&vec![b'a'; 1023]);
    check_roundtrip(&vec![b'a'; 1024]);
    check_roundtrip(&vec![b'a'; 1 << 16]);
    check_roundtrip(&vec![0u8; 1 << 20]);

    let mut bytes = Vec::with_capacity(1 << 17);
    let mut state = 0x1234_5678u32;
    for i in 0..(1 << 17) {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        bytes.push((state >> 24) as u8 ^ (i as u8));
    }
    check_roundtrip(&bytes);

    let mut mixed = Vec::with_capacity(1 << 18);
    for i in 0..(1 << 18) {
        mixed.push(match i & 31 {
            0..=23 => b'A' + (i % 4) as u8,
            _ => (i >> 3) as u8,
        });
    }
    check_roundtrip(&mixed);
}

#[test]
fn handmade_tags() {
    let small_literal = [3, 8, b'a', b'b', b'c'];
    assert_eq!(snipsnap::decompress(&small_literal).unwrap(), b"abc");

    let mut long_literal = vec![61, 60 << 2, 60];
    long_literal.extend(0u8..61);
    assert_eq!(
        snipsnap::decompress(&long_literal).unwrap(),
        (0u8..61).collect::<Vec<_>>()
    );

    let copy_1 = [8, 12, b'a', b'b', b'c', b'd', 1, 4];
    assert_eq!(snipsnap::decompress(&copy_1).unwrap(), b"abcdabcd");

    let mut copy_1_far = vec![32];
    raw_literal(b"abcdefghijklmnop", &mut copy_1_far);
    copy_1_far.extend([1, 16]);
    raw_literal(b"qrstuvwxyzAB", &mut copy_1_far);
    let copy_1_far_expected = b"abcdefghijklmnopabcdqrstuvwxyzAB";
    assert_eq!(
        snipsnap::decompress(&copy_1_far).unwrap(),
        copy_1_far_expected
    );
    assert_eq!(
        snap::raw::Decoder::new()
            .decompress_vec(&copy_1_far)
            .unwrap(),
        copy_1_far_expected
    );

    let copy_2 = [8, 12, b'a', b'b', b'c', b'd', 14, 4, 0];
    assert_eq!(snipsnap::decompress(&copy_2).unwrap(), b"abcdabcd");

    let copy_4 = [8, 12, b'a', b'b', b'c', b'd', 15, 4, 0, 0, 0];
    assert_eq!(snipsnap::decompress(&copy_4).unwrap(), b"abcdabcd");

    let run = [12, 0, b'x', 29, 1];
    assert_eq!(snipsnap::decompress(&run).unwrap(), b"xxxxxxxxxxxx");

    for offset in 1u8..=7 {
        let copy_len = 20usize;
        let total_len = usize::from(offset) + copy_len;
        let mut block = vec![total_len as u8];
        block.push((usize::from(offset) - 1) as u8 * 4);
        block.extend((0..offset).map(|i| b'a' + i));
        block.push((((copy_len - 1) << 2) | 2) as u8);
        block.extend([offset, 0]);

        let mut expected = Vec::with_capacity(total_len);
        expected.extend((0..offset).map(|i| b'a' + i));
        for i in 0..copy_len {
            expected.push(b'a' + (i % usize::from(offset)) as u8);
        }
        assert_eq!(
            snipsnap::decompress(&block).unwrap(),
            expected,
            "offset {offset}"
        );
    }

    for offset in [8usize, 9, 10, 15, 16, 17, 37] {
        let copy_len = 38usize;
        let total_len = offset + copy_len;
        let seed = (0..offset)
            .map(|i| b'a' + (i % 23) as u8)
            .collect::<Vec<_>>();
        let mut block = Vec::new();
        raw_header(total_len, &mut block);
        raw_literal(&seed, &mut block);
        raw_copy2(copy_len, offset, &mut block);

        let mut expected = seed.clone();
        for i in 0..copy_len {
            expected.push(expected[i]);
        }

        assert_eq!(
            snipsnap::decompress(&block).unwrap(),
            expected,
            "copy2 len 38 offset {offset}"
        );
        assert_eq!(
            snap::raw::Decoder::new().decompress_vec(&block).unwrap(),
            expected,
            "snap oracle len38 offset {offset}"
        );
    }

    for offset in [9usize, 15, 16, 17, 20, 31, 32, 33, 63] {
        let copy_len = 64usize;
        let total_len = offset + copy_len;
        let seed = (0..offset)
            .map(|i| b'a' + (i % 23) as u8)
            .collect::<Vec<_>>();
        let mut block = Vec::new();
        raw_header(total_len, &mut block);
        raw_literal(&seed, &mut block);
        raw_copy2(copy_len, offset, &mut block);

        let mut expected = seed.clone();
        for i in 0..copy_len {
            expected.push(expected[i]);
        }

        assert_eq!(
            snipsnap::decompress(&block).unwrap(),
            expected,
            "copy2 len 64 offset {offset}"
        );
        assert_eq!(
            snap::raw::Decoder::new().decompress_vec(&block).unwrap(),
            expected,
            "snap oracle offset {offset}"
        );
    }

    let offset = 79usize;
    let copies = 2048usize;
    let total_len = offset + copies * 64;
    let seed = (0..offset)
        .map(|i| b'a' + (i % 23) as u8)
        .collect::<Vec<_>>();
    let mut block = Vec::new();
    raw_header(total_len, &mut block);
    raw_literal(&seed, &mut block);
    for _ in 0..copies {
        raw_copy2(64, offset, &mut block);
    }

    let mut expected = Vec::with_capacity(total_len);
    for i in 0..total_len {
        expected.push(seed[i % offset]);
    }

    assert_eq!(snipsnap::decompress(&block).unwrap(), expected);
    assert_eq!(
        snap::raw::Decoder::new().decompress_vec(&block).unwrap(),
        expected
    );
}

#[test]
fn invalid_inputs_are_rejected() {
    assert_eq!(
        snipsnap::decompress(b"").unwrap_err(),
        snipsnap::Error::Empty
    );
    assert_eq!(
        snipsnap::decompress(&[0x80]).unwrap_err(),
        snipsnap::Error::Header
    );
    assert!(snipsnap::decompress(&[1]).is_err());
    assert!(snipsnap::decompress(&[2, 4, b'a']).is_err());
    assert!(snipsnap::decompress(&[4, 1, 0]).is_err());
    assert!(snipsnap::decompress(&[4, 1, 8]).is_err());
    assert!(snipsnap::decompress(&[4, 14, 1]).is_err());
    assert!(snipsnap::decompress(&[4, 15, 1, 0, 0]).is_err());

    let compressed = enc(b"abcdef");
    let mut too_small = [MaybeUninit::uninit(); 5];
    assert!(matches!(
        snipsnap::decompress_into_uninit(&compressed, &mut too_small),
        Err(snipsnap::Error::OutputTooSmall { given: 5, min: 6 })
    ));
}
