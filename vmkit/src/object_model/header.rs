use std::marker::PhantomData;

use crate::VirtualMachine;
use easy_bitfield::*;

use super::object::ADDRESS_BASED_HASHING;

/// Offset from allocation pointer to the actual object start.
pub const OBJECT_REF_OFFSET: isize = 8;
/// Object header behind object.
pub const OBJECT_HEADER_OFFSET: isize = -OBJECT_REF_OFFSET;
pub const HASHCODE_OFFSET: isize = -(OBJECT_REF_OFFSET + size_of::<usize>() as isize);

pub const METADATA_BIT_LIMIT: usize = if ADDRESS_BASED_HASHING { usize::BITS as usize - 2 } else { usize::BITS as usize - 1 };

pub type MetadataField = BitField<usize, usize, 0, METADATA_BIT_LIMIT, false>;
pub type HashStateField = BitField<usize, HashState, { MetadataField::NEXT_BIT }, 2, false>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashState {
    Unhashed,
    Hashed,
    HashedAndMoved,
}

impl FromBitfield<usize> for HashState {
    fn from_bitfield(value: usize) -> Self {
        match value {
            0 => Self::Unhashed,
            1 => Self::Hashed,
            2 => Self::HashedAndMoved,
            _ => unreachable!(),
        }
    }

    fn from_i64(value: i64) -> Self {
        match value {
            0 => Self::Unhashed,
            1 => Self::Hashed,
            2 => Self::HashedAndMoved,
            _ => unreachable!(),
        }
    }
}

impl ToBitfield<usize> for HashState {
    fn to_bitfield(self) -> usize {
        match self {
            Self::Unhashed => 0,
            Self::Hashed => 1,
            Self::HashedAndMoved => 2,
        }
    }

    fn one() -> Self {
        Self::Hashed
    }

    fn zero() -> Self {
        Self::Unhashed
    }
}

pub struct HeapObjectHeader<VM: VirtualMachine> {
    pub metadata: AtomicBitfieldContainer<usize>,
    pub marker: PhantomData<VM>,
}

impl<VM: VirtualMachine> HeapObjectHeader<VM> {
    pub fn new(metadata: VM::Metadata) -> Self {
        Self {
            metadata: AtomicBitfieldContainer::new(metadata.to_bitfield()),
            marker: PhantomData,
        }
    }

    pub fn hash_state(&self) -> HashState {
        if !ADDRESS_BASED_HASHING {
            return HashState::Unhashed;
        }
        self.metadata.read::<HashStateField>()
    }

    pub fn set_hash_state(&self, state: HashState) {
        self.metadata.update_synchronized::<HashStateField>(state);
    }

    pub fn metadata(&self) -> VM::Metadata {
        VM::Metadata::from_bitfield(self.metadata.read::<MetadataField>() as _)
    }

    pub fn set_metadata(&self, metadata: VM::Metadata) {
        self.metadata
            .update_synchronized::<MetadataField>(metadata.to_bitfield() as _);
    }
}
