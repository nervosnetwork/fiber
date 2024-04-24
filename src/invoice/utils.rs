use arcode::bitbit::{BitReader, BitWriter, MSB};
use arcode::{ArithmeticDecoder, ArithmeticEncoder, EOFKind, Model};
use bech32::{u5, FromBase32, WriteBase32};
use std::io::{Cursor, Result as IoResult};

/// Encodes bytes and returns the compressed form
pub(crate) fn ar_encompress(data: &[u8]) -> IoResult<Vec<u8>> {
    let mut model = Model::builder().num_bits(8).eof(EOFKind::EndAddOne).build();
    let mut compressed_writer = BitWriter::new(Cursor::new(vec![]));
    let mut encoder = ArithmeticEncoder::new(48);
    for &sym in data {
        encoder.encode(sym as u32, &model, &mut compressed_writer)?;
        model.update_symbol(sym as u32);
    }

    encoder.encode(model.eof(), &model, &mut compressed_writer)?;
    encoder.finish_encode(&mut compressed_writer)?;
    compressed_writer.pad_to_byte()?;

    // retrieves the bytes from the writer.
    // This will be cleaner when bitbit updates.
    // Not necessary if using files or a stream
    Ok(compressed_writer.get_ref().get_ref().clone())
}

/// Decompresses the data
pub(crate) fn ar_decompress(data: &[u8]) -> IoResult<Vec<u8>> {
    let mut model = Model::builder().num_bits(8).eof(EOFKind::EndAddOne).build();
    let mut input_reader = BitReader::<_, MSB>::new(data);
    let mut decoder = ArithmeticDecoder::new(48);
    let mut decompressed_data = vec![];

    while !decoder.finished() {
        let sym = decoder.decode(&model, &mut input_reader)?;
        model.update_symbol(sym);
        decompressed_data.push(sym as u8);
    }

    decompressed_data.pop(); // remove the EOF
    Ok(decompressed_data)
}

/// Construct the invoice's HRP and signatureless data into a preimage to be hashed.
pub fn construct_invoice_preimage(hrp_bytes: &[u8], data_without_signature: &[u5]) -> Vec<u8> {
    let mut preimage = Vec::<u8>::from(hrp_bytes);

    let mut data_part = Vec::from(data_without_signature);
    let overhang = (data_part.len() * 5) % 8;
    if overhang > 0 {
        // add padding if data does not end at a byte boundary
        data_part.push(u5::try_from_u8(0).unwrap());

        // if overhang is in (1..3) we need to add u5(0) padding two times
        if overhang < 3 {
            data_part.push(u5::try_from_u8(0).unwrap());
        }
    }

    preimage.extend_from_slice(
        &Vec::<u8>::from_base32(&data_part)
            .expect("No padding error may occur due to appended zero above."),
    );
    preimage
}

/// Converts a stream of bytes written to it to base32. On finalization the according padding will
/// be applied. That means the results of writing two data blocks with one or two `BytesToBase32`
/// converters will differ.
pub(crate) struct BytesToBase32<'a, W: WriteBase32 + 'a> {
    /// Target for writing the resulting `u5`s resulting from the written bytes
    writer: &'a mut W,
    /// Holds all unwritten bits left over from last round. The bits are stored beginning from
    /// the most significant bit. E.g. if buffer_bits=3, then the byte with bits a, b and c will
    /// look as follows: [a, b, c, 0, 0, 0, 0, 0]
    buffer: u8,
    /// Amount of bits left over from last round, stored in buffer.
    buffer_bits: u8,
}

impl<'a, W: WriteBase32> BytesToBase32<'a, W> {
    /// Create a new bytes-to-base32 converter with `writer` as  a sink for the resulting base32
    /// data.
    pub fn new(writer: &'a mut W) -> BytesToBase32<'a, W> {
        BytesToBase32 {
            writer,
            buffer: 0,
            buffer_bits: 0,
        }
    }

    /// Add more bytes to the current conversion unit
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), W::Err> {
        for b in bytes {
            self.append_u8(*b)?;
        }
        Ok(())
    }

    pub fn append_u8(&mut self, byte: u8) -> Result<(), W::Err> {
        // Write first u5 if we have to write two u5s this round. That only happens if the
        // buffer holds too many bits, so we don't have to combine buffer bits with new bits
        // from this rounds byte.
        if self.buffer_bits >= 5 {
            self.writer
                .write_u5(u5::try_from_u8((self.buffer & 0b11111000) >> 3).expect("<32"))?;
            self.buffer <<= 5;
            self.buffer_bits -= 5;
        }

        // Combine all bits from buffer with enough bits from this rounds byte so that they fill
        // a u5. Save remaining bits from byte to buffer.
        let from_buffer = self.buffer >> 3;
        let from_byte = byte >> (3 + self.buffer_bits); // buffer_bits <= 4

        self.writer
            .write_u5(u5::try_from_u8(from_buffer | from_byte).expect("<32"))?;
        self.buffer = byte << (5 - self.buffer_bits);
        self.buffer_bits += 3;

        Ok(())
    }

    pub fn finalize(mut self) -> Result<(), W::Err> {
        self.inner_finalize()?;
        core::mem::forget(self);
        Ok(())
    }

    fn inner_finalize(&mut self) -> Result<(), W::Err> {
        // There can be at most two u5s left in the buffer after processing all bytes, write them.
        if self.buffer_bits >= 5 {
            self.writer
                .write_u5(u5::try_from_u8((self.buffer & 0b11111000) >> 3).expect("<32"))?;
            self.buffer <<= 5;
            self.buffer_bits -= 5;
        }

        if self.buffer_bits != 0 {
            self.writer
                .write_u5(u5::try_from_u8(self.buffer >> 3).expect("<32"))?;
        }

        Ok(())
    }
}

impl<'a, W: WriteBase32> Drop for BytesToBase32<'a, W> {
    fn drop(&mut self) {
        self.inner_finalize()
            .expect("Unhandled error when finalizing conversion on drop. User finalize to handle.")
    }
}
