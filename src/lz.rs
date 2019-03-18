use byteorder::BE;
use byteorder::ByteOrder;
use super::aux::UncheckedSliceExt;
use super::bits::Bits;
use super::huffman::HuffmanDecoder;
use super::huffman::HuffmanEncoder;
use super::matchfinder::DecoderMFBucket;
use super::matchfinder::EncoderMFBucket;
use super::matchfinder::MatchResult;
use super::mtf::MTFDecoder;
use super::mtf::MTFEncoder;

const LZ_ROID_ENCODING_ARRAY: [(u8, u8, u16); super::LZ_MF_BUCKET_ITEM_SIZE] = include!(
    concat!(env!("OUT_DIR"), "/", "LZ_ROID_ENCODING_ARRAY.txt"));
const LZ_ROID_DECODING_ARRAY: [(u16, u8); super::LZ_ROID_SIZE] = include!(
    concat!(env!("OUT_DIR"), "/", "LZ_ROID_DECODING_ARRAY.txt"));

pub struct LZCfg {
    pub match_depth: usize,
    pub lazy_match_depth1: usize,
    pub lazy_match_depth2: usize,
    pub lazy_match_depth3: usize,
}
pub struct LZEncoder {
    buckets: Vec<EncoderMFBucket>,
    mtfs:    Vec<MTFEncoder>,
    words:   [u16; 65536],
}
pub struct LZDecoder {
    buckets: Vec<DecoderMFBucket>,
    mtfs:    Vec<MTFDecoder>,
    words:   [u16; 65536],
}

pub enum MatchItem {
    LastWord,
    Match   {reduced_offset: u16, match_len: u8},
    Literal {mtf_symbol: u8},
}

impl LZEncoder {
    pub fn new() -> LZEncoder {
        LZEncoder {
            buckets: (0 .. 256).map(|_| EncoderMFBucket::new()).collect(),
            mtfs:    (0 .. 256).map(|_| MTFEncoder::new()).collect(),
            words:   [0; 65536],
        }
    }

    pub fn forward(&mut self, forward_len: usize) {
        self.buckets.iter_mut().for_each(|bucket| bucket.forward(forward_len));
    }

    pub unsafe fn encode(&mut self, cfg: &LZCfg, sbuf: &[u8], tbuf: &mut [u8], spos: usize) -> (usize, usize) {
        let mut spos = spos;
        let mut tpos = 0;
        let mut match_items = Vec::with_capacity(super::LZ_CHUNK_SIZE);

        macro_rules! sc {
            ($off:expr) => {{
                sbuf.nocheck()[(spos as isize + $off as isize) as usize]
            }}
        }
        macro_rules! sw {
            ($off:expr) => {{
                (sc!($off - 1) as u16) << 8 | (sc!($off) as u16)
            }}
        }

        // huff1:
        //  0 .. 256 => mtf_symbol
        //  256 => last_word
        //  257 => unused
        //  ... => roid
        let mut huff_weights1 = [0u32; 258 + super::LZ_MATCH_MAX_LEN + 1];
        let mut huff_weights2 = [0u32; super::LZ_ROID_SIZE];

        // start Lempel-Ziv encoding
        while spos < sbuf.len() && match_items.len() < match_items.capacity() {
            let match_result = self.buckets.nocheck()[sc!(-1) as usize].find_match(
                sbuf,
                spos,
                cfg.match_depth);

            // find match
            let mut matched = false;
            if let Some(MatchResult {reduced_offset, match_len, match_len_expected}) = match_result {
                let has_lazy_match =
                    self.buckets.nocheck()[sc!(0) as usize].has_lazy_match(sbuf, spos + 1,
                            match_len + 1 + (match_len_expected == match_len) as usize, cfg.lazy_match_depth1) ||
                    self.buckets.nocheck()[sc!(1) as usize].has_lazy_match(sbuf, spos + 2,
                            match_len + 1 + (match_len_expected == match_len) as usize - (
                                self.words.nocheck()[sw!(-1) as usize] == sw!(1)) as usize, cfg.lazy_match_depth2) ||
                    self.buckets.nocheck()[sc!(2) as usize].has_lazy_match(sbuf, spos + 3,
                            match_len + 2 + (match_len_expected == match_len) as usize - (
                                self.words.nocheck()[sw!(-1) as usize] == sw!(1) ||
                                self.words.nocheck()[sw!(0)  as usize] == sw!(2)) as usize * 2, cfg.lazy_match_depth3);

                if !has_lazy_match {
                    let encoding_match_len = match match_len_expected.cmp(&match_len) {
                        std::cmp::Ordering::Equal   => super::LZ_MATCH_MIN_LEN,
                        std::cmp::Ordering::Greater => match_len + 1,
                        std::cmp::Ordering::Less    => match_len,
                    };

                    match_items.push(MatchItem::Match {
                        reduced_offset: reduced_offset as u16,
                        match_len: encoding_match_len as u8,
                    });
                    self.buckets.nocheck_mut()[sc!(-1) as usize].update(sbuf, spos, match_len);
                    spos += match_len;
                    matched = true;

                    let roid = LZ_ROID_ENCODING_ARRAY.nocheck()[reduced_offset].0 as usize;
                    huff_weights1.nocheck_mut()[258 + encoding_match_len] += 1;
                    huff_weights2.nocheck_mut()[roid] += 1;
                }
            }

            if !matched {
                self.buckets.nocheck_mut()[sc!(-1) as usize].update(sbuf, spos, 0);

                let last_word_expected = self.words.nocheck()[sw!(-1) as usize];
                if last_word_expected == sw!(1) {
                    match_items.push(MatchItem::LastWord);
                    spos += 2;
                    huff_weights1[256] += 1; // count huffman
                } else {
                    let mtf_symbol = self.mtfs.nocheck_mut()[sc!(-1) as usize].encode(sc!(0));
                    match_items.push(MatchItem::Literal {mtf_symbol});
                    spos += 1;
                    huff_weights1.nocheck_mut()[mtf_symbol as usize] += 1; // count huffman
                }
            }
            self.words.nocheck_mut()[sw!(-3) as usize] = sw!(-1);
        }

        // encode match_items_len
        BE::write_u32(std::slice::from_raw_parts_mut(tbuf.get_unchecked_mut(tpos), 4), match_items.len() as u32);
        tpos += 4;

        // start Huffman encoding
        let huff_encoder1 = HuffmanEncoder::from_symbol_weight_vec(&huff_weights1, 15);
        let huff_encoder2 = HuffmanEncoder::from_symbol_weight_vec(&huff_weights2, 8);
        let mut bits = Bits::new();
        for huff_symbol_bits_lens in &[huff_encoder1.get_symbol_bits_lens(), huff_encoder2.get_symbol_bits_lens()] {
            for i in 0 .. huff_symbol_bits_lens.len() / 2 {
                tbuf.nocheck_mut()[tpos + i] =
                    huff_symbol_bits_lens.nocheck()[i * 2 + 0] * 16 +
                    huff_symbol_bits_lens.nocheck()[i * 2 + 1];
            }
            tpos += huff_symbol_bits_lens.len() / 2;
        }

        for match_item in &match_items {
            match match_item {
                &MatchItem::Literal {mtf_symbol} => {
                    huff_encoder1.encode_to_bits(mtf_symbol as u16, &mut bits);
                },
                &MatchItem::LastWord => {
                    huff_encoder1.encode_to_bits(256, &mut bits);
                },
                &MatchItem::Match {reduced_offset, match_len} => {
                    let (roid, robitlen, robits) = LZ_ROID_ENCODING_ARRAY.nocheck()[reduced_offset as usize];
                    huff_encoder1.encode_to_bits(match_len as u16 + 258, &mut bits);
                    huff_encoder2.encode_to_bits(roid as u16, &mut bits);
                    bits.put(robitlen, robits as u64);
                }
            }
            if bits.len() >= 32 {
                BE::write_u32(std::slice::from_raw_parts_mut(tbuf.get_unchecked_mut(tpos), 4), bits.get(32) as u32);
                tpos += 4;
            }
        }
        let num_unaligned_bits = 8 - bits.len() % 8;
        bits.put(num_unaligned_bits, 0);

        while bits.len() > 0 {
            tbuf[tpos] = bits.get(8) as u8;
            tpos += 1;
        }
        return (spos, tpos);
    }
}

impl LZDecoder {
    pub fn new() -> LZDecoder {
        return LZDecoder {
            buckets: (0 .. 256).map(|_| DecoderMFBucket::new()).collect(),
            mtfs:    (0 .. 256).map(|_| MTFDecoder::new()).collect(),
            words:   [0; 65536],
        };
    }

    pub fn forward(&mut self, forward_len: usize) {
        self.buckets.iter_mut().for_each(|bucket| bucket.forward(forward_len));
    }

    pub unsafe fn decode(&mut self, tbuf: &[u8], sbuf: &mut [u8], spos: usize) -> Result<(usize, usize), ()> {
        let mut spos = spos;
        let mut tpos = 0;

        macro_rules! sc {
            ($off:expr) => {{
                sbuf.nocheck()[(spos as isize + $off as isize) as usize]
            }}
        }
        macro_rules! sw {
            ($off:expr) => {{
                (sc!($off - 1) as u16) << 8 | (sc!($off) as u16)
            }}
        }
        macro_rules! sc_set {
            ($off:expr, $c:expr) => {{
                let c = $c;
                sbuf.nocheck_mut()[(spos as isize + $off as isize) as usize] = c;
            }}
        }
        macro_rules! sw_set {
            ($off:expr, $w:expr) => {{
                let w = $w;
                sc_set!($off - 1, (w >> 8) as u8);
                sc_set!($off - 0, (w >> 0) as u8);
            }}
        }

        // decode match_items_len
        let match_items_len = BE::read_u32(std::slice::from_raw_parts(tbuf.get_unchecked(tpos), 4)) as usize;
        tpos += 4;

        // start decoding
        let mut huff_symbol_bits_lens1 = [0u8; 258 + super::LZ_MATCH_MAX_LEN + 1];
        let mut huff_symbol_bits_lens2 = [0u8; super::LZ_ROID_SIZE];
        for huff_symbol_bits_lens in [&mut huff_symbol_bits_lens1[..], &mut huff_symbol_bits_lens2[..]].iter_mut() {
            for i in 0 .. huff_symbol_bits_lens.len() / 2 {
                huff_symbol_bits_lens.nocheck_mut()[i * 2 + 0] = tbuf.nocheck()[tpos + i] / 16;
                huff_symbol_bits_lens.nocheck_mut()[i * 2 + 1] = tbuf.nocheck()[tpos + i] % 16;
            }
            tpos += huff_symbol_bits_lens.len() / 2;
        }

        let huff_decoder1 = HuffmanDecoder::from_symbol_bits_lens(&huff_symbol_bits_lens1);
        let huff_decoder2 = HuffmanDecoder::from_symbol_bits_lens(&huff_symbol_bits_lens2);
        let mut bits = Bits::new();
        for _ in 0 .. match_items_len {
            if bits.len() < 32 {
                bits.put(32, BE::read_u32(std::slice::from_raw_parts(tbuf.get_unchecked(tpos), 4)) as u64);
                tpos += 4;
            }

            let leader = huff_decoder1.decode_from_bits(&mut bits);
            if leader < 256 {
                sc_set!(0, self.mtfs.nocheck_mut()[sc!(-1) as usize].decode(leader as u8));
                self.buckets.nocheck_mut()[sc!(-1) as usize].update(spos, 0);
                spos += 1;

            } else if leader == 256 {
                sw_set!(1, self.words.nocheck()[sw!(-1) as usize]);
                self.buckets.nocheck_mut()[sc!(-1) as usize].update(spos, 0);
                spos += 2;

            } else {
                let roid = huff_decoder2.decode_from_bits(&mut bits) as usize;
                if roid as usize >= super::LZ_ROID_SIZE {
                    Err(())?;
                }
                let (robase, robitlen) = LZ_ROID_DECODING_ARRAY.nocheck()[roid];
                let reduced_offset = robase + bits.get(robitlen) as u16;
                let (
                    match_pos,
                    match_len_expected,
                ) = self.buckets.nocheck()[sc!(-1) as usize].get_match_pos_and_match_len(reduced_offset);

                let encoding_match_len = leader as usize - 258;
                let match_len = match encoding_match_len {
                    super::LZ_MATCH_MIN_LEN => match_len_expected,
                    l if l <= match_len_expected => encoding_match_len - 1,
                    l if l >  match_len_expected => encoding_match_len,
                    _ => unreachable!(),
                };
                if match_len < super::LZ_MATCH_MIN_LEN || match_len > super::LZ_MATCH_MAX_LEN {
                    Err(())?;
                }
                super::mem::copy_fast(sbuf, match_pos, spos, match_len);
                self.buckets.nocheck_mut()[sc!(-1) as usize].update(spos, match_len);
                spos += match_len;
            }
            self.words.nocheck_mut()[sw!(-3) as usize] = sw!(-1);

            if spos >= sbuf.len() {
                break;
            }
        }
        // (spos+match_len) may overflow, but it is safe because of sentinels
        Ok((std::cmp::min(spos, sbuf.len()), std::cmp::min(tpos, tbuf.len())))
    }
}
