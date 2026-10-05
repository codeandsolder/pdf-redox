//! Minimal raw-PDF construction helpers for engine-native unit tests.

use crate::{Error, Result};
use std::{collections::BTreeMap, io::Write as _};

pub struct ClassicPdfBuilder {
    bytes: Vec<u8>,
    offsets: BTreeMap<u32, usize>,
}

impl ClassicPdfBuilder {
    pub fn new() -> Self {
        Self {
            bytes: b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec(),
            offsets: BTreeMap::new(),
        }
    }

    pub fn object(&mut self, number: u32, body: &[u8]) -> Result<()> {
        if self.offsets.contains_key(&number) {
            return Err(Error::Invalid(
                "duplicate test PDF object number".to_owned(),
            ));
        }
        self.offsets.insert(number, self.bytes.len());
        writeln!(&mut self.bytes, "{number} 0 obj")?;
        self.bytes.extend_from_slice(body);
        self.bytes.extend_from_slice(b"\nendobj\n");
        Ok(())
    }

    pub fn stream(&mut self, number: u32, dictionary_entries: &[u8], data: &[u8]) -> Result<()> {
        if self.offsets.contains_key(&number) {
            return Err(Error::Invalid(
                "duplicate test PDF object number".to_owned(),
            ));
        }
        self.offsets.insert(number, self.bytes.len());
        writeln!(&mut self.bytes, "{number} 0 obj")?;
        write!(&mut self.bytes, "<< /Length {}", data.len())?;
        if !dictionary_entries.is_empty() {
            self.bytes.push(b' ');
            self.bytes.extend_from_slice(dictionary_entries);
        }
        self.bytes.extend_from_slice(b" >>\nstream\n");
        self.bytes.extend_from_slice(data);
        self.bytes.extend_from_slice(b"\nendstream\nendobj\n");
        Ok(())
    }

    pub fn finish(mut self, root: u32) -> Result<Vec<u8>> {
        if !self.offsets.contains_key(&root) {
            return Err(Error::Invalid("test PDF root object is missing".to_owned()));
        }
        let max_object = self.offsets.keys().next_back().copied().unwrap_or(root);
        let xref_offset = self.bytes.len();
        writeln!(&mut self.bytes, "xref")?;
        writeln!(&mut self.bytes, "0 {}", max_object + 1)?;
        self.bytes.extend_from_slice(b"0000000000 65535 f \n");
        for number in 1..=max_object {
            if let Some(offset) = self.offsets.get(&number) {
                writeln!(&mut self.bytes, "{offset:010} 00000 n ")?;
            } else {
                self.bytes.extend_from_slice(b"0000000000 00000 f \n");
            }
        }
        self.bytes.extend_from_slice(b"trailer\n<< /Size ");
        write!(&mut self.bytes, "{}", max_object + 1)?;
        write!(
            &mut self.bytes,
            " /Root {root} 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
        )?;
        Ok(self.bytes)
    }
}
