use mmtk::util::{conversions::raw_align_up, Address};

use crate::VirtualMachine;

use super::align::align_allocation_no_fill;

/// Thread-local allocation buffer.
pub struct TLAB {
    pub cursor: Address,
    pub limit: Address,
}

impl TLAB {
    pub fn new() -> Self {
        Self {
            cursor: Address::ZERO,
            limit: Address::ZERO,
        }
    }

    pub fn allocate<VM: VirtualMachine>(
        &mut self,
        size: usize,
        alignment: usize,
        offset: usize,
    ) -> Address {
        let size = raw_align_up(size, alignment);
        let result = align_allocation_no_fill::<VM>(self.cursor, alignment, offset);
        if result + size > self.limit {
            return Address::ZERO;
        } else {
            self.cursor = result.add(size);
            return result;
        }
    }

    pub fn rebind(&mut self, cursor: Address, limit: Address) {
        self.cursor = cursor;
        self.limit = limit;
    }

    pub fn reset(&mut self) {
        self.cursor = Address::ZERO;
        self.limit = Address::ZERO;
    }

    pub fn take(&mut self) -> (Address, Address) {
        let cursor = self.cursor;
        let limit = self.limit;
        self.reset();
        (cursor, limit)
    }
}
