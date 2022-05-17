//use crate::structure::guid::{GUID, /*EntityId, GuidPrefix*/ };
use std::{collections::BTreeMap, convert::TryInto, fmt};

use bit_vec::BitVec;
use enumflags2::BitFlags;
use bytes::BytesMut;
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

use crate::{
  dds::ddsdata::DDSData,
  messages::submessages::{
    submessage_elements::serialized_payload::SerializedPayload,
    submessages::{DATAFRAG_Flags, DataFrag},
  },
  structure::{cache_change::ChangeKind, sequence_number::SequenceNumber, time::Timestamp},
  RepresentationIdentifier,
};

// This is for the assembly of a single object
struct AssemblyBuffer {
  buffer_bytes: BytesMut,
  #[allow(dead_code)] // This module is still WiP
  fragment_count: usize,
  received_bitmap: BitVec,

  #[allow(dead_code)] // This module is still WiP
  created_time: Timestamp,
  modified_time: Timestamp,
}

impl AssemblyBuffer {
  pub fn new(data_size: u32, fragment_size: u16) -> Self {
    debug!(
      "new AssemblyBuffer data_size={} frag_size={}",
      data_size, fragment_size
    );
    // TODO: Check that fragment size <= data_size
    // TODO: Check that fragment_size is not zero
    let data_size: usize = data_size.try_into().unwrap();
    // we have unwrap here, but it will succeed as long as usize >= u32

    let mut buffer_bytes = BytesMut::with_capacity(data_size);
    buffer_bytes.resize(data_size, 0); //TODO: Can we replace this with faster (and unsafer) .set_len and live with
                                       // uninitialized data?

    let frag_size = usize::from(fragment_size);
    // fragment count formula from RTPS spec v2.5 Section 8.3.8.3.5
    let fragment_count = (data_size / frag_size) + (if data_size % frag_size == 0 { 0 } else { 1 });
    let now = Timestamp::now();

    Self {
      buffer_bytes,
      fragment_count,
      received_bitmap: BitVec::from_elem(fragment_count, false),
      created_time: now,
      modified_time: now,
    }
  }

  pub fn insert_frags(&mut self, datafrag: &DataFrag, frag_size: u16) {
    // TODO: Sanity checks? E.g. datafrag.fragment_size == frag_size
    //let payload_header = 4; // RepresentationIdentifier + RepresentationOptions
    let frag_size = usize::from(frag_size); // - payload_header;
    let frags_in_subm = usize::from(datafrag.fragments_in_submessage);
    let fragment_starting_num: usize = u32::from(datafrag.fragment_starting_num)
      .try_into()
      .unwrap();
    let start_frag_from_0 = fragment_starting_num - 1; // number of first fragment in this DataFrag, indexing from 0

    debug!(
      "insert_frags: datafrag.writer_sn = {:?}, frag_size = {:?}, datafrag.fragment_size = {:?}, datafrag.fragment_starting_num = {:?}, \
      datafrag.fragments_in_submessage = {:?}, datafrag.data_size = {:?}",
      datafrag.writer_sn, frag_size, datafrag.fragment_size, datafrag.fragment_starting_num,
      datafrag.fragments_in_submessage, datafrag.data_size
    );

    let room_for_sp_header = // account for header fields inside serializedPayload
      if start_frag_from_0 == 0 { 4 } else { 0 };

    // unwrap: u32 should fit into usize
    let mut from_byte = start_frag_from_0 * frag_size;
    // Last fragment might be smaller than fragment size
    let to_before_byte = if fragment_starting_num < self.fragment_count {
      from_byte + (frags_in_subm * frag_size)
    } else {
      from_byte + datafrag.serialized_payload.value.len()
    };
    from_byte += room_for_sp_header; // modify from_byte to account for header

    debug!(
      "insert_frags: from_byte = {:?}, to_before_byte = {:?}",
      from_byte, to_before_byte
    );

    debug!(
      "insert_frags: dataFrag.serializedPayload.value.len = {:?}",
      datafrag.serialized_payload.value.len()
    );

    if start_frag_from_0 == 0 {
      debug!("Filling bytes 0..4 from serialized_payload header");
      self.buffer_bytes.as_mut()[0..2].copy_from_slice(
        &datafrag
          .serialized_payload
          .representation_identifier
          .to_bytes(),
      );
      self.buffer_bytes.as_mut()[2..4]
        .copy_from_slice(&datafrag.serialized_payload.representation_options);
    }

    self.buffer_bytes.as_mut()[from_byte..to_before_byte]
      .copy_from_slice(&datafrag.serialized_payload.value);

    for f in 0..frags_in_subm {
      self.received_bitmap.set(start_frag_from_0 + f, true);
    }
    self.modified_time = Timestamp::now();
  }

  pub fn is_complete(&self) -> bool {
    self.received_bitmap.all() // return if all are received
  }
}

// Assembles fragments from a single (remote) Writer
// So there is only one sequence of SNs
pub(crate) struct FragmentAssembler {
  fragment_size: u16, // number of bytes per fragment. Each writer must select one constant value.
  assembly_buffers: BTreeMap<SequenceNumber, AssemblyBuffer>,
}

impl fmt::Debug for FragmentAssembler {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("FragmentAssembler - fields omitted")
      // insert field printing here, if you really need it.
      .finish()
  }
}

impl FragmentAssembler {
  pub fn new(fragment_size: u16) -> Self {
    debug!("new FragmentAssember. frag_size = {}", fragment_size);
    Self {
      fragment_size,
      assembly_buffers: BTreeMap::new(),
    }
  }

  // Returns completed DDSData, when complete, and disposes the assembly buffer.
  pub fn new_datafrag(
    &mut self,
    datafrag: &DataFrag,
    flags: BitFlags<DATAFRAG_Flags>,
  ) -> Option<DDSData> {
    //let rep_id = datafrag.serialized_payload.representation_identifier;
    let writer_sn = datafrag.writer_sn;
    let frag_size = self.fragment_size;

    let abuf = self
      .assembly_buffers
      .entry(datafrag.writer_sn)
      .or_insert_with(|| AssemblyBuffer::new(datafrag.data_size, frag_size));

    abuf.insert_frags(datafrag, frag_size);

    if abuf.is_complete() {
      debug!("new_datafrag: COMPLETED FRAGMENT");
      if let Some(abuf) = self.assembly_buffers.remove(&writer_sn) {
        // Return what we have assembled.
        let rep_id = RepresentationIdentifier::from_bytes(&abuf.buffer_bytes[0..2]).ok()?;
        let ser_data_or_key = SerializedPayload::new(rep_id, abuf.buffer_bytes[4..].to_vec());
        let ddsdata = if flags.contains(DATAFRAG_Flags::Key) {
          DDSData::new_disposed_by_key(ChangeKind::NotAliveDisposed, ser_data_or_key)
        } else {
          // it is data
          DDSData::new(ser_data_or_key)
        };
        Some(ddsdata) // completed data from fragments
      } else {
        error!("Assembly buffer mysteriously lost");
        None
      }
    } else {
      debug!("new_dataFrag: FRAGMENT NOT COMPLETED YET");
      None
    }
  }
}
