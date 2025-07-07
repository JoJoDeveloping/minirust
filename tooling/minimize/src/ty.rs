use crate::*;

impl<'tcx> Ctxt<'tcx> {
    pub fn pointee_info_of(&mut self, ty: rs::Ty<'tcx>, span: rs::Span) -> PointeeInfo {
        let layout = self.rs_layout_of(ty);
        let inhabited = !layout.is_uninhabited();
        let freeze = ty.is_freeze(self.tcx, self.typing_env());
        let unpin = ty.is_unpin(self.tcx, self.typing_env());

        if layout.is_sized() {
            let size = translate_size(layout.size());
            let align = translate_align(layout.align().abi);
            let layout = LayoutStrategy::Sized(size, align);
            // The ranges are sorted in descending order, but we want them to be in ascending order.
            let nonfreeze_bytes = self.nonfreeze_bytes_in_sized_ty(ty, span).iter().rev().collect();
            return PointeeInfo { layout, inhabited, freeze: UnsafeCellStrategy::Sized { bytes: nonfreeze_bytes }, unpin };
        }

        // Handle Unsized types:
        match ty.kind() {
            &rs::TyKind::Slice(elem_ty) => {
                let elem_layout = self.rs_layout_of(elem_ty);
                let elem_nonfreeze_bytes = self.nonfreeze_bytes_in_sized_ty(elem_ty, span).iter().rev().collect();
                let size = translate_size(elem_layout.size());
                let align = translate_align(elem_layout.align().abi);
                let layout = LayoutStrategy::Slice(size, align);
                PointeeInfo { layout, inhabited, freeze: UnsafeCellStrategy::Slice { element: elem_nonfreeze_bytes }, unpin }
            }
            &rs::TyKind::Str => {
                // Treat `str` like `[u8]`.
                let layout = LayoutStrategy::Slice(Size::from_bytes_const(1), Align::ONE);
                PointeeInfo { layout, inhabited, freeze: UnsafeCellStrategy::Slice { element: List::new() }, unpin }
            }
            &rs::TyKind::Dynamic(_, _, rs::DynKind::Dyn) => {
                let layout = LayoutStrategy::TraitObject(self.get_trait_name(ty));
                PointeeInfo { layout, inhabited, freeze: UnsafeCellStrategy::TraitObject { is_freeze: freeze }, unpin }
            }
            _ => rs::span_bug!(span, "encountered unimplemented unsized type: {ty}"),
        }
    }

    pub fn pointee_info_of_smir(&mut self, ty: smir::Ty, span: rs::Span) -> PointeeInfo {
        self.pointee_info_of(smir::internal(self.tcx, ty), span)
    }

    pub fn translate_ty_smir(&mut self, ty: smir::Ty, span: rs::Span) -> Type {
        self.translate_ty(smir::internal(self.tcx, ty), span)
    }

    pub fn nonfreeze_bytes_in_sized_ty(&mut self, ty: rs::Ty<'tcx>, span: rs::Span) -> List<(Offset, Offset)> {
        match ty.kind() {
            rs::TyKind::Bool => List::new(),
            rs::TyKind::Int(_) => List::new(),
            rs::TyKind::Uint(_) => List::new(),
            rs::TyKind::RawPtr(..) => List::new(),
            rs::TyKind::Ref(..) => List::new(),
            rs::TyKind::Adt(adt_def, _) if adt_def.is_box() => List::new(),
            rs::TyKind::FnPtr(..) => List::new(),
            rs::TyKind::Never => List::new(),
            rs::TyKind::Tuple(ts) => {
                let layout = self.rs_layout_of(ty);
                ts.iter().enumerate().flat_map(|(i, ty)| {
                    let offset = translate_size(layout.fields().offset(i));
                    self.nonfreeze_bytes_in_sized_ty(ty, span).map(|(start, end)| (start + offset, end + offset))
                }).collect::<List<(Size, Size)>>()
            }
            rs::TyKind::Adt(adt_def, _) if adt_def.is_unsafe_cell() => {
                let layout = self.rs_layout_of(ty);
                let size = layout.size();
                list![(Size::from_bytes_const(0), translate_size(size))]
            }
            rs::TyKind::Adt(adt_def, sref) if adt_def.is_struct() => {
                let layout = self.rs_layout_of(ty);
                adt_def.non_enum_variant()
                    .fields
                    .iter_enumerated()
                    .flat_map(|(i, field)| {
                        let ty = field.ty(self.tcx, sref);
                        let offset = layout.fields().offset(i.into());
                        let offset = translate_size(offset);
                        self.nonfreeze_bytes_in_sized_ty(ty, span).map(|(start, end)| (start + offset, end + offset))
                    })
                    .collect::<List<(Size, Size)>>()
            }
            rs::TyKind::Adt(adt_def, _sref) if adt_def.is_union() || adt_def.is_enum() => {
                // If any variant has an `UnsafeCell` somewhere in it, the whole range will be non-freeze.
                let ty_is_freeze = ty.is_freeze(self.tcx, self.typing_env());
                let layout = self.rs_layout_of(ty);
                let size = translate_size(layout.size());

                if ty_is_freeze {
                    List::new()
                } else {
                    list!((Size::from_bytes_const(0), size))
                }
            }
            rs::TyKind::Array(elem_ty, c) => {
                let range = self.nonfreeze_bytes_in_sized_ty(*elem_ty, span);
                if !range.is_empty() {
                    let layout = self.rs_layout_of(*elem_ty);
                    let size = translate_size(layout.size());
                    let count = Int::from(c.try_to_target_usize(self.tcx).unwrap());
                    let ranges = List::from_elem(0, count);

                    ranges.iter().enumerate().flat_map(|(i, _)| {
                        let offset = size * i.into();
                        range.map(|(start, end)| (start + offset, end + offset))
                    }).collect()
                } else {
                    List::new()
                }
            },
            x => rs::span_bug!(span, "TyKind not supported: {x:?}"),

        }
    }

    pub fn translate_ty(&mut self, ty: rs::Ty<'tcx>, span: rs::Span) -> Type {
        if let Some(mini_ty) = self.ty_cache.get(&ty) {
            return *mini_ty;
        }

        let mini_ty = match ty.kind() {
            rs::TyKind::Bool => Type::Bool,
            rs::TyKind::Int(t) => {
                let sz = rs::abi::Integer::from_int_ty(&self.tcx, *t).size();
                Type::Int(IntType { size: translate_size(sz), signed: Signedness::Signed })
            }
            rs::TyKind::Uint(t) => {
                let sz = rs::abi::Integer::from_uint_ty(&self.tcx, *t).size();
                Type::Int(IntType { size: translate_size(sz), signed: Signedness::Unsigned })
            }
            rs::TyKind::Tuple(ts) => {
                let layout = self.rs_layout_of(ty);
                let size = translate_size(layout.size());
                let align = translate_align(layout.align().abi);

                let fields = ts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let t = self.translate_ty(t, span);
                        let offset = layout.fields().offset(i);
                        let offset = translate_size(offset);

                        (offset, t)
                    })
                    .collect::<Vec<_>>();

                build::tuple_ty(&fields, size, align)
            }
            rs::TyKind::Adt(adt_def, _) if adt_def.is_box() => {
                let ty = ty.expect_boxed_ty();
                let pointee = self.pointee_info_of(ty, span);
                Type::Ptr(PtrType::Box { pointee })
            }
            rs::TyKind::Adt(adt_def, sref) if adt_def.is_struct() => {
                let (fields, size, align) = self.translate_non_enum_adt(ty, *adt_def, sref, span);
                build::tuple_ty(&fields.iter().collect::<Vec<_>>(), size, align)
            }
            rs::TyKind::Adt(adt_def, sref) if adt_def.is_union() => {
                let (fields, size, align) = self.translate_non_enum_adt(ty, *adt_def, sref, span);
                let chunks = calc_chunks(fields, size);
                Type::Union { fields, size, align, chunks }
            }
            rs::TyKind::Adt(adt_def, sref) if adt_def.is_enum() =>
                self.translate_enum(ty, *adt_def, sref, span),
            rs::TyKind::Ref(_, ty, mutbl) => {
                let pointee = self.pointee_info_of(*ty, span);
                let mutbl = translate_mutbl(*mutbl);
                Type::Ptr(PtrType::Ref { pointee, mutbl })
            }
            rs::TyKind::RawPtr(ty, _mutbl) => {
                let pointee = self.pointee_info_of(*ty, span);
                Type::Ptr(PtrType::Raw { meta_kind: pointee.layout.meta_kind() })
            }
            rs::TyKind::Array(ty, c) => {
                let count = Int::from(c.try_to_target_usize(self.tcx).unwrap());
                let elem = GcCow::new(self.translate_ty(*ty, span));
                Type::Array { elem, count }
            }
            rs::TyKind::FnPtr(..) => Type::Ptr(PtrType::FnPtr),
            rs::TyKind::Never =>
                build::enum_ty::<u8>(&[], Discriminator::Invalid, build::size(0), build::align(1)),
            rs::TyKind::Slice(ty) => {
                let elem = GcCow::new(self.translate_ty(*ty, span));
                Type::Slice { elem }
            }
            rs::TyKind::Str => {
                // Treat `str` like `[u8]`.
                let elem = GcCow::new(Type::Int(IntType {
                    size: Size::from_bytes_const(1),
                    signed: Signedness::Unsigned,
                }));
                Type::Slice { elem }
            }
            rs::TyKind::Dynamic(_, _, rs::DynKind::Dyn) =>
                Type::TraitObject(self.get_trait_name(ty)),
            x => rs::span_bug!(span, "TyKind not supported: {x:?}"),
        };
        self.ty_cache.insert(ty, mini_ty);
        mini_ty
    }

    /// Constructs the fields of a given variant.
    pub fn translate_adt_variant_fields(
        &mut self,
        shape: &rs::FieldsShape<rs::FieldIdx>,
        variant: &rs::VariantDef,
        sref: rs::GenericArgsRef<'tcx>,
        span: rs::Span,
    ) -> Fields {
        variant
            .fields
            .iter_enumerated()
            .map(|(i, field)| {
                let ty = field.ty(self.tcx, sref);
                // Field types can be non-normalized even if the ADT type was normalized
                // (due to associated types on the fields).
                let ty = self.tcx.normalize_erasing_regions(self.typing_env(), ty);
                let ty = self.translate_ty(ty, span);
                let offset = shape.offset(i.into());
                let offset = translate_size(offset);

                (offset, ty)
            })
            .collect()
    }

    fn translate_non_enum_adt(
        &mut self,
        ty: rs::Ty<'tcx>,
        adt_def: rs::AdtDef<'tcx>,
        sref: rs::GenericArgsRef<'tcx>,
        span: rs::Span,
    ) -> (Fields, Size, Align) {
        let layout = self.rs_layout_of(ty);
        let fields = self.translate_adt_variant_fields(
            layout.fields(),
            adt_def.non_enum_variant(),
            sref,
            span,
        );
        let size = translate_size(layout.size());
        let align = translate_align(layout.align().abi);

        (fields, size, align)
    }
}

pub fn translate_mutbl(mutbl: rs::Mutability) -> Mutability {
    match mutbl {
        rs::Mutability::Mut => Mutability::Mutable,
        rs::Mutability::Not => Mutability::Immutable,
    }
}

pub fn translate_mutbl_smir(mutbl: smir::Mutability) -> Mutability {
    match mutbl {
        smir::Mutability::Mut => Mutability::Mutable,
        smir::Mutability::Not => Mutability::Immutable,
    }
}

pub fn translate_size(size: rs::Size) -> Size {
    Size::from_bytes_const(size.bytes())
}

pub fn translate_align(align: rs::Align) -> Align {
    Align::from_bytes(align.bytes()).unwrap()
}

pub fn translate_calling_convention(conv: rs::Conv) -> CallingConvention {
    match conv {
        rs::Conv::C => CallingConvention::C,
        rs::Conv::Rust => CallingConvention::Rust,
        _ => todo!(),
    }
}
