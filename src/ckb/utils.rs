use arcode::bitbit::{BitReader, BitWriter, MSB};
use arcode::{ArithmeticDecoder, ArithmeticEncoder, EOFKind, Model};
use std::io::{Cursor, Result as IoResult};

/// Encodes bytes and returns the compressed form
pub(crate) fn ar_encode(data: &[u8]) -> IoResult<Vec<u8>> {
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
pub(crate) fn ar_decode(data: &[u8]) -> IoResult<Vec<u8>> {
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
