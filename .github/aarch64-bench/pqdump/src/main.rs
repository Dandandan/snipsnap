// Dumps the Snappy-compressed pages of a Parquet file as raw blocks:
// [u32 column][u32 page type][u32 len][block] ..., and prints a summary.
#![allow(deprecated)]
use parquet::basic::Compression;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::format::{PageHeader, PageType};
use parquet::thrift::TSerializable;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use thrift::protocol::TCompactInputProtocol;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let path = &a[1];
    let reader = SerializedFileReader::new(File::open(path).unwrap()).unwrap();
    let md = reader.metadata();
    let schema = md.file_metadata().schema_descr();
    let mut file = File::open(path).unwrap();
    let mut out = Vec::new();
    let mut codecs = BTreeMap::new();
    let mut per_col: BTreeMap<usize, (String, usize, u64, u64)> = BTreeMap::new();
    println!("row groups {}, columns {}", md.num_row_groups(), schema.num_columns());
    for rg in 0..md.num_row_groups() {
        let g = md.row_group(rg);
        for ci in 0..g.num_columns() {
            let c = g.column(ci);
            *codecs.entry(format!("{:?}", c.compression())).or_insert(0) += 1;
            if c.compression() != Compression::SNAPPY { continue; }
            let (start, len) = c.byte_range();
            let mut chunk = vec![0; len as usize];
            file.seek(SeekFrom::Start(start)).unwrap();
            file.read_exact(&mut chunk).unwrap();
            let mut off = 0usize;
            while off < chunk.len() {
                let mut cur = std::io::Cursor::new(&chunk[off..]);
                let Ok(h) = ({ let mut p = TCompactInputProtocol::new(&mut cur); PageHeader::read_from_in_protocol(&mut p) }) else { break };
                off += cur.position() as usize;
                let end = off + h.compressed_page_size as usize;
                let mut body = &chunk[off..end];
                let mut compressed = true;
                if let Some(v2) = &h.data_page_header_v2 {
                    let lv = (v2.repetition_levels_byte_length + v2.definition_levels_byte_length) as usize;
                    body = &body[lv..];
                    compressed = v2.is_compressed.unwrap_or(true);
                }
                let ty = match h.type_ { PageType::DATA_PAGE => 0u32, PageType::DICTIONARY_PAGE => 1, PageType::DATA_PAGE_V2 => 2, _ => 3 };
                if compressed && !body.is_empty() && ty < 3 {
                    let dec = snap::raw::Decoder::new().decompress_vec(body).expect("snappy");
                    let e = per_col.entry(ci).or_insert((schema.column(ci).name().to_string(), 0, 0, 0));
                    e.1 += 1; e.2 += body.len() as u64; e.3 += dec.len() as u64;
                    out.extend_from_slice(&(ci as u32).to_le_bytes());
                    out.extend_from_slice(&ty.to_le_bytes());
                    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
                    out.extend_from_slice(body);
                }
                off = end;
            }
        }
    }
    println!("codecs {:?}", codecs);
    let (mut tc, mut tu, mut tp) = (0u64, 0u64, 0usize);
    for (ci, (name, pages, c, u)) in &per_col {
        tc += c; tu += u; tp += pages;
        println!("{ci:3} {name:<22} pages {pages:5} comp {c:11} raw {u:11} ratio {:.2} avg raw/page {}", *u as f64 / *c as f64, u / *pages as u64);
    }
    println!("total pages {tp} comp {tc} raw {tu}");
    std::fs::write(&a[2], out).unwrap();
}
