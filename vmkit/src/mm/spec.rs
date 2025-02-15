/// Define a VM metadata spec. This is essentially bitfields for storing object metadata.
/// Using this your VM can declare side metadata or local (header) metadata with ease.
///
/// This macro is copied from mmtk-core source code.
#[macro_export]
macro_rules! define_vm_metadata_spec {
        ($(#[$outer:meta])*$spec_name: ident, $is_global: expr, $log_num_bits: expr, $side_min_obj_size: expr) => {
            $(#[$outer])*
            pub struct $spec_name($crate::mmtk::util::metadata::MetadataSpec);
            impl $spec_name {
                /// The number of bits (in log2) that are needed for the spec.
                pub const LOG_NUM_BITS: usize = $log_num_bits;

                /// Whether this spec is global or local. For side metadata, the binding needs to make sure
                /// global specs are laid out after another global spec, and local specs are laid
                /// out after another local spec. Otherwise, there will be an assertion failure.
                pub const IS_GLOBAL: bool = $is_global;

                /// Declare that the VM uses in-header metadata for this metadata type.
                /// For the specification of the `bit_offset` argument, please refer to
                /// the document of `[crate::util::metadata::header_metadata::HeaderMetadataSpec.bit_offset]`.
                /// The binding needs to make sure that the bits used for a spec in the header do not conflict with
                /// the bits of another spec (unless it is specified that some bits may be reused).
                pub const fn in_header(bit_offset: isize) -> Self {
                    Self($crate::mmtk::util::metadata::MetadataSpec::InHeader($crate::mmtk::util::metadata::header_metadata::HeaderMetadataSpec {
                        bit_offset,
                        num_of_bits: 1 << Self::LOG_NUM_BITS,
                    }))
                }

                /// Declare that the VM uses side metadata for this metadata type,
                /// and the side metadata is the first of its kind (global or local).
                /// The first global or local side metadata should be declared with `side_first()`,
                /// and the rest side metadata should be declared with `side_after()` after a defined
                /// side metadata of the same kind (global or local). Logically, all the declarations
                /// create two list of side metadata, one for global, and one for local.
                pub const fn side_first() -> Self {
                    if Self::IS_GLOBAL {
                        Self($crate::mmtk::util::metadata::MetadataSpec::OnSide($crate::mmtk::util::metadata::side_metadata::SideMetadataSpec {
                            name: stringify!($spec_name),
                            is_global: Self::IS_GLOBAL,
                            offset: $crate::mmtk::util::metadata::side_metadata::GLOBAL_SIDE_METADATA_VM_BASE_OFFSET,
                            log_num_of_bits: Self::LOG_NUM_BITS,
                            log_bytes_in_region: $side_min_obj_size as usize,
                        }))
                    } else {
                        Self($crate::mmtk::util::metadata::MetadataSpec::OnSide($crate::mmtk::util::metadata::side_metadata::SideMetadataSpec {
                            name: stringify!($spec_name),
                            is_global: Self::IS_GLOBAL,
                            offset: $crate::mmtk::util::metadata::side_metadata::LOCAL_SIDE_METADATA_VM_BASE_OFFSET,
                            log_num_of_bits: Self::LOG_NUM_BITS,
                            log_bytes_in_region: $side_min_obj_size as usize,
                        }))
                    }
                }

                /// Declare that the VM uses side metadata for this metadata type,
                /// and the side metadata should be laid out after the given side metadata spec.
                /// The first global or local side metadata should be declared with `side_first()`,
                /// and the rest side metadata should be declared with `side_after()` after a defined
                /// side metadata of the same kind (global or local). Logically, all the declarations
                /// create two list of side metadata, one for global, and one for local.
                pub const fn side_after(spec: &$crate::mmtk::util::metadata::MetadataSpec) -> Self {
                    assert!(spec.is_on_side());
                    let side_spec = spec.extract_side_spec();
                    assert!(side_spec.is_global == Self::IS_GLOBAL);
                    Self($crate::mmtk::util::metadata::MetadataSpec::OnSide($crate::mmtk::util::metadata::side_metadata::SideMetadataSpec {
                        name: stringify!($spec_name),
                        is_global: Self::IS_GLOBAL,
                        offset: $crate::mmtk::util::metadata::side_metadata::SideMetadataOffset::layout_after(side_spec),
                        log_num_of_bits: Self::LOG_NUM_BITS,
                        log_bytes_in_region: $side_min_obj_size as usize,
                    }))
                }

                /// Return the inner `[crate::util::metadata::MetadataSpec]` for the metadata type.
                pub const fn as_spec(&self) -> &$crate::mmtk::util::metadata::MetadataSpec {
                    &self.0
                }

                /// Return the number of bits for the metadata type.
                pub const fn num_bits(&self) -> usize {
                    1 << $log_num_bits
                }
            }
            impl std::ops::Deref for $spec_name {
                type Target = $crate::mmtk::util::metadata::MetadataSpec;
                fn deref(&self) -> &Self::Target {
                    self.as_spec()
                }
            }
        };
    }
