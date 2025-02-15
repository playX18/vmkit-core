use easy_bitfield::BitFieldTrait;
use vmkit::object_model::header::{HashState, HashStateField};

fn main() {
    let encoded = HashStateField::encode(HashState::HashedAndMoved);
    println!("{:x}", encoded);
    println!("{}", HashStateField::NEXT_BIT);
    println!("{:?}", HashStateField::decode(encoded));
}
