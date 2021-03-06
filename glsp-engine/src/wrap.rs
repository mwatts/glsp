#![allow(clippy::needless_lifetimes)]
#![allow(clippy::type_complexity)]

use super::class::{Class, Obj};
use super::code::{Coro, GFn};
use super::collections::{Arr, Deque, DequeAccess, DequeOps, Str, Tab};
use super::engine::{
    glsp, stock_syms::*, RData, RFn, RGlobal, RGlobalRef, RGlobalRefMut, RRef, RRefMut, RRoot, Sym,
};
use super::error::{GError, GResult};
use super::eval::{EnvMode, Expander};
use super::gc::{Raw, Root, Slot};
use super::iter::{GIter, GIterLen, Iterable};
use super::val::{Num, Val};
use smallvec::SmallVec;
use std::any::type_name;
use std::cell::Ref;
use std::cmp::{min, Ordering};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::convert::{TryFrom, TryInto};
use std::error::Error;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::hash::{BuildHasher, Hash};
use std::io::Write;
use std::iter::{Extend, IntoIterator};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut, Index, IndexMut};
use std::path::{Path, PathBuf};
use std::slice::SliceIndex;
use std::{i128, i16, i32, i64, i8, isize, slice, str, u128, u16, u32, u64, u8, usize};

/*
this module defines:
- the IntoVal and FromVal traits, with all of their built-in implementations
- the pub(crate) wrap() function and its supporting traits, which are used to convert arbitrary
  Rust functions into boxed type-erased functions which can be called by GameLisp

todos:
- should FromVal support Option<T> and Rest<T> in tuples?
- should we support Either<T, U>? this will require argument type conversions to fail more cheaply
  than they currently do, perhaps returning a dedicated error type rather than GError
    - this would also enable us to provide a method Val::is::<T>(&self) -> bool
- should FromVal::from_val accept any parameter which implements AsRef<Val>, so that it
  can accept either Val or &Val when called explicitly?
    - for now, i'm reluctant to add the extra monomorphization cost
- it ought to be possible to add a write_back() method to FromArgRef, and implement that method
  for any &mut T: FromVal + IntoVal, as well as &mut str etc. this would enable us to support
  more Rust apis without any manual translation. however...
    - i'm skeptical there would be much demand for it, and perf would be poor
    - it would require us to remove the blanket implementations which let the user move
      RData on and off the heap using t.into_val() and T::from_val()
    - it could only support arrs, strs and tabs - not, e.g., &mut Sym
    - we probably want to encourage use of RData rather than FromVal/ToVal
    - non-self &mut references are fairly uncommon in Rust apis anyway, and when they do exist
      they're usually only there as a performance optimization
- a lesser version of that feature would be to support &T args for all T: FromVal, constructing
  the T on the stack. however, this would require us to yank our blanket FromVal impl for all
  'static types, which would be a shame
*/

//-------------------------------------------------------------------------------------------------
// marker traits
//-------------------------------------------------------------------------------------------------

/*
if this style of specialization gets yanked from rustc, we have various backup plans...

IntoVal:

    implement two traits on wrappable function pointers for transforming their return value:
        glsp::bind_rfn("name", name.move_rdata())?;
        glsp::bind_rfn("name", name.move_inner_rdata())?;

    move_inner_rdata() would be implemented for standard library types like Option<T>, Vec<T>,
    Result<T, E> (with a dynamic "specialization" for GResult), and so on. move_rdata() could
    use type_name::<T>() to lint against moving types like Option<T> onto the heap, with a
    move_rdata_force() method on the same trait to override the lint.

    we would also implement analogous methods .into_rdata() and .into_inner_rdata() on the
    return types themselves

FromVal, FromArg, FromArgRef:

    we could potentially emit code in the macro-generated impls which manually checks
    whether *any* argument is an RData before converting it, in which case it's tested for
    a matching type id, proceeding with the original conversion if there's a mismatch.
    (this would obviously carry a non-trivial performance cost)

    the specialization for &T where T: RGlobal would be abandoned. we could either capture
    globals using an explicit wrapper type, probably called Res<T> and ResMut<T>, or we could
    go back to requiring the user to define their global types using a macro
*/

#[doc(hidden)]
#[rustc_unsafe_specialization_marker]
pub trait StaticMarker: 'static {}
impl<T: 'static + ?Sized> StaticMarker for T {}

#[doc(hidden)]
#[rustc_unsafe_specialization_marker]
pub trait RGlobalMarker: RGlobal {}
impl<T: RGlobal> RGlobalMarker for T {}

#[doc(hidden)]
#[rustc_unsafe_specialization_marker]
pub trait ErrorMarker: Error {}
impl<T: Error> ErrorMarker for T {}

#[doc(hidden)]
#[rustc_unsafe_specialization_marker]
pub trait HashEqMarker: Hash + Eq {}
impl<T: Hash + Eq> HashEqMarker for T {}

#[doc(hidden)]
#[rustc_unsafe_specialization_marker]
pub trait BuildHasherDefaultMarker: BuildHasher + Default {}
impl<T: BuildHasher + Default> BuildHasherDefaultMarker for T {}

#[doc(hidden)]
#[rustc_unsafe_specialization_marker]
pub trait OrdMarker: Ord {}
impl<T: Ord> OrdMarker for T {}

//-------------------------------------------------------------------------------------------------
// IntoVal and FromVal: definitions and blanket impls
//-------------------------------------------------------------------------------------------------

/**
A type which can be converted to a GameLisp value.

Many functions in the `glsp` crate receive a generic parameter `T: IntoVal`. This enables those
functions to accept various different Rust types, silently converting those types into a
[`Val`](enum.Val.html).

```
# extern crate glsp_engine as glsp;
# use glsp::*;
# 
# Engine::new().run(|| {
# let arr = arr![];
# glsp::bind_global("numbers", 1)?;
# 
glsp::set_global("numbers", (0, 1, 2, 3, 4))?;
arr.push("an example string")?;
# 
# Ok(()) }).unwrap();
```

Invoking a type's [`into_val` method](#tymethod.into_val) is usually the most convenient way to
produce a `Val`. `IntoVal` is part of the [prelude](prelude/index.html), so there's no need to
import it into scope.

```
# extern crate glsp_engine as glsp;
# use glsp::*;
# 
# Engine::new().run(|| {
# 
let thousand = 10.0_f64.powf(3.0).into_val()?;
# 
# Ok(()) }).unwrap();
```

We provide `IntoVal` implementations for many common types, including all of Rust's primitive
integer and floating-point types; primitive Rust types like `()` and `bool`; most standard
collections, including arrays, slices and tuples; `Root` and `RRoot`; type-erased enums like
`Deque` and `Callable`; shared and mutable references to all of the above; and shared references
to primitive GameLisp types like `&Arr` and `&GFn`.

`Option` and `Result` have special handling, which can be useful for return values:

- `Option` will produce a nil value if it's `None`, or otherwise call `into_val()`
  for its `Some` value.

- `Result` will trigger an error if it's `Err`, or otherwise call `into_val()`
  for its `Ok` value. Non-GameLisp errors are fully supported.

There is a default `IntoVal` implementation for all `'static` types. This implementation moves
the Rust value onto the garbage-collected heap, wrapping it in an [`RData`](struct.RData.html).
The conversion returns a [`Val::RData`](enum.Val.html).

Because this default implementation applies to all `'static` types, including those defined in
external crates, it's possible to move most foreign types onto the GameLisp heap. For example,
to construct a standard Rust `File` and transfer its ownership to GameLisp, you can simply call:

```no_run
# use glsp_engine::*;
# use std::fs::File;
# 
# Engine::new().run(|| {
# 
File::open("my_file.png").into_val()?;
# 
# Ok(()) }).unwrap();
```

If you'd like one of your own types to be represented in GameLisp by something other than an
`rdata` (for example, converting an enum to a GameLisp symbol, or a tuple struct to a GameLisp
array), you can implement `IntoVal` for your type. This will enable automatic conversions when
your type is passed to a generic function like [`glsp::set_global`](fn.set_global.html). It will
also automatically convert your type into GameLisp data when it's used as an `RFn` return value.

**Implementing `IntoVal` for your own types currently requires the `min_specialization` nightly
feature. Enable it by writing `#![feature(min_specialization)]` at the top of your crate's
`main.rs` or `lib.rs` file.**

```
# #![feature(min_specialization)]
# extern crate glsp_engine as glsp;
# use glsp::*;
# 
# Engine::new().run(|| {
# 
struct Rgb(u8, u8, u8);

impl IntoVal for Rgb {
    fn into_val(self) -> GResult<Val> {
        let Rgb(r, g, b) = self;
        arr![r, g, b].into_val()
    }
}

fn light_sea_green() -> Rgb {
    Rgb(32, 178, 170)
}

glsp::bind_rfn("light-sea-green", &light_sea_green)?;

//calling (light-sea-green) from a GameLisp script will
//automatically call Rgb::into_val(), converting the function's
//Rgb return value into an array of three integers, (32 178 178)
# 
# Ok(()) }).unwrap();
```

When implementing `IntoVal` for your own types, it's generally a good idea to also provide
implementations for shared and mutable references to your type. It makes the borrow checker
easier to deal with, and it enables you to bind Rust functions which return references. It's
usually straightforward:

```
# #![feature(min_specialization)]
# extern crate glsp_engine as glsp;
# use glsp::*;
# 
# Engine::new().run(|| {
# 
# struct MyType;
# 
impl<'a> IntoVal for &'a MyType {
    fn into_val(self) -> GResult<Val> {
        //...the actual conversion...
        # bail!()
    }
}

impl<'a> IntoVal for &'a mut MyType {
    fn into_val(self) -> GResult<Val> {
        (self as &MyType).into_val()
    }
}

impl IntoVal for MyType {
    fn into_val(self) -> GResult<Val> {
        (&self).into_val()
    }
}
# 
# Ok(()) }).unwrap();
```
*/

#[rustc_specialization_trait]
pub trait IntoVal: Sized {
    fn into_val(self) -> GResult<Val>;

    #[doc(hidden)]
    fn into_slot(self) -> GResult<Slot> {
        self.into_val()?.into_slot()
    }
}

impl<T: StaticMarker> IntoVal for T {
    #[inline]
    default fn into_val(self) -> GResult<Val> {
        Ok(Val::RData(glsp::rdata(self)))
    }

    /*
    we need to be conservative here. if a more-specific impl only implements into_val(), its
    into_slot() implementation will be this one, so we can't assume that we want the
    RData-like behaviour.
    */

    #[doc(hidden)]
    #[inline]
    default fn into_slot(self) -> GResult<Slot> {
        self.into_val()?.into_slot()
    }
}

/**
A type which can be converted from a GameLisp value.

Many functions in the `glsp` crate have a generic return value `R: FromVal`. They can
automatically convert their return value to many different Rust types.

```
# extern crate glsp_engine as glsp;
# use glsp::*;
# 
# Engine::new().run(|| {
# 
# let arr = arr!["doctest"];
# glsp::bind_global("numbers", (1_i32, 2_i32, 3_i32))?;
# 
let numbers: Vec<u8> = glsp::global("numbers")?;
let text: Root<Str> = arr.pop()?;
# 
# Ok(()) }).unwrap();
```

Writing `T::from_val(v)?` is usually the most convenient way to destructure a `Val`. `FromVal`
is part of the [prelude](prelude/index.html), so there's usually no need to import it into scope.

```
# extern crate glsp_engine as glsp;
# use glsp::*;
# 
# Engine::new().run(|| {
# let val = Val::Flo(100.0);
# 
let f = f64::from_val(&val)?;
# 
# Ok(()) }).unwrap();
```

We provide `FromVal` implementations for many common types, including all of Rust's primitive
integer and floating-point types; primitive Rust types like `bool`; most standard collections,
including arrays, slices and tuples; `Root` and `RRoot`; type-erased enums like `Deque` and
`Callable`; and owned string types, including `PathBuf`, `OsString` and `CString`.

You can also implement `FromVal` for your own types, which will enable them to take advantage of
automatic conversions when they're [bound as an `RFn` parameter](fn.rfn.html).

**Implementing `FromVal` for your own types currently requires the `min_specialization` nightly
feature. Enable it by writing `#![feature(min_specialization)]` at the top of your crate's
`main.rs` or `lib.rs` file.**

```
# #![feature(min_specialization)]
# extern crate glsp_engine as glsp;
# use glsp::*;
# 
# Engine::new().run(|| {
# 
struct Rgb(u8, u8, u8);

impl FromVal for Rgb {
    fn from_val(val: &Val) -> GResult<Rgb> {
        let (r, g, b) = <(u8, u8, u8)>::from_val(val)?;
        Ok(Rgb(r, g, b))
    }
}

fn describe_rgb(rgb: Rgb) {
    let Rgb(r, g, b) = rgb;
    println!("Red: {}\nGreen: {}\nBlue: {}", r, g, b);
}

glsp::bind_rfn("describe-rgb", &describe_rgb)?;

//calling (describe-rgb '(32 178 178)) from a GameLisp script will
//automatically invoke Rgb::from_val, converting the array of three
//integers into an Rgb struct
# 
# Ok(()) }).unwrap();
```
*/

#[rustc_specialization_trait]
pub trait FromVal: Sized + StaticMarker {
    fn from_val(val: &Val) -> GResult<Self>;

    #[doc(hidden)]
    fn from_slot(slot: &Slot) -> GResult<Self> {
        Self::from_val(&slot.root())
    }
}

//we won't be able to switch this on until associated type specialization is supported
//(todo? it might never be supported; it's not part of min_specialization). otherwise, we'd
//be forced to have a dynamic Temp type for FromArg, the way we do for FromArgRef.
/*
impl<T: StaticMarker> FromVal for T {
    #[inline]
    default fn from_val(val: &Val) -> GResult<Self> {
        todo!() //rdata.take()
    }

    //we need to be conservative here. see IntoVal::into_slot, above
    #[doc(hidden)]
    #[inline]
    default fn from_slot(slot: &Slot) -> GResult<Self> {
        Self::from_val(&slot.root())
    }
}
*/

//-------------------------------------------------------------------------------------------------
// IntoVal implementations
//-------------------------------------------------------------------------------------------------

impl IntoVal for Val {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(self)
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        Ok(Slot::from_val(&self))
    }
}

impl<'a> IntoVal for &'a Val {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok((*self).clone())
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        Ok(Slot::from_val(self))
    }
}

impl<'a> IntoVal for &'a mut Val {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok((*self).clone())
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        Ok(Slot::from_val(self))
    }
}

impl IntoVal for Slot {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(match self {
            Slot::Nil => Val::Nil,
            Slot::Int(i) => Val::Int(i),
            Slot::Char(c) => Val::Char(c),
            Slot::Flo(f) => Val::Flo(f),
            Slot::Bool(b) => Val::Bool(b),
            Slot::Sym(s) => Val::Sym(s),
            Slot::RFn(r) => Val::RFn(r.into_root()),
            Slot::Arr(a) => Val::Arr(a.into_root()),
            Slot::Str(s) => Val::Str(s.into_root()),
            Slot::Tab(t) => Val::Tab(t.into_root()),
            Slot::GIter(g) => Val::GIter(g.into_root()),
            Slot::Obj(o) => Val::Obj(o.into_root()),
            Slot::Class(c) => Val::Class(c.into_root()),
            Slot::GFn(c) => Val::GFn(c.into_root()),
            Slot::Coro(c) => Val::Coro(c.into_root()),
            Slot::RData(r) => Val::RData(r.into_root()),
        })
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        Ok(self)
    }
}

impl<'a> IntoVal for &'a Slot {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (*self).clone().into_val()
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        Ok((*self).clone())
    }
}

impl<'a> IntoVal for &'a mut Slot {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (*self).clone().into_val()
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        Ok((*self).clone())
    }
}

impl<T: IntoVal> IntoVal for Option<T> {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        match self {
            Some(src) => src.into_val(),
            None => Ok(Val::Nil),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        match self {
            Some(src) => src.into_slot(),
            None => Ok(Slot::Nil),
        }
    }
}

impl<'a, T> IntoVal for &'a Option<T>
where
    &'a T: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        self.as_ref().into_val()
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        self.as_ref().into_slot()
    }
}

impl<'a, T> IntoVal for &'a mut Option<T>
where
    &'a mut T: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        self.as_mut().into_val()
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        self.as_mut().into_slot()
    }
}

impl<T: IntoVal, E: ErrorMarker + StaticMarker> IntoVal for Result<T, E> {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        match self {
            Ok(src) => src.into_val(),
            Err(err) => {
                /*
                we're forced to dynamically "specialize" for GResult here, so that
                GError::MacroNoOp will propagate properly rather than being promoted
                to a true error. we could use actual specialization instead (which
                would eliminate the allocation here), but i prefer to avoid it
                */

                let dyn_err: &(dyn Error + 'static) = &err;
                if dyn_err.is::<GError>() {
                    let dyn_err_boxed: Box<dyn Error + 'static> = Box::new(err);
                    let g_err: GError = *dyn_err_boxed.downcast::<GError>().unwrap();
                    Err(g_err)
                } else {
                    Err(error!("IntoVal encountered {}", type_name::<E>()).with_source(err))
                }
            }
        }
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        self.into_val()?.into_slot()
    }
}

impl IntoVal for () {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Nil)
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        Ok(Slot::Nil)
    }
}

impl<'a> IntoVal for &'a () {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Nil)
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        Ok(Slot::Nil)
    }
}

impl<'a> IntoVal for &'a mut () {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Nil)
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        Ok(Slot::Nil)
    }
}

macro_rules! impl_into_val_infallible {
    ($self_type:ty, $variant:ident) => {
        impl IntoVal for $self_type {
            #[inline]
            fn into_val(self) -> GResult<Val> {
                Ok(Val::$variant(self.into()))
            }

            #[doc(hidden)]
            #[inline]
            fn into_slot(self) -> GResult<Slot> {
                Ok(Slot::$variant(self.into()))
            }
        }

        impl<'a> IntoVal for &'a $self_type {
            #[inline]
            fn into_val(self) -> GResult<Val> {
                Ok(Val::$variant((*self).into()))
            }

            #[doc(hidden)]
            #[inline]
            fn into_slot(self) -> GResult<Slot> {
                Ok(Slot::$variant((*self).into()))
            }
        }

        impl<'a> IntoVal for &'a mut $self_type {
            #[inline]
            fn into_val(self) -> GResult<Val> {
                Ok(Val::$variant((*self).into()))
            }

            #[doc(hidden)]
            #[inline]
            fn into_slot(self) -> GResult<Slot> {
                Ok(Slot::$variant((*self).into()))
            }
        }
    };
}

impl_into_val_infallible!(i8, Int);
impl_into_val_infallible!(i16, Int);
impl_into_val_infallible!(i32, Int);
impl_into_val_infallible!(u8, Int);
impl_into_val_infallible!(u16, Int);
impl_into_val_infallible!(f32, Flo);
impl_into_val_infallible!(char, Char);
impl_into_val_infallible!(bool, Bool);
impl_into_val_infallible!(Sym, Sym);

macro_rules! impl_into_val_root {
    ($t:ident) => {
        impl IntoVal for Root<$t> {
            #[inline]
            fn into_val(self) -> GResult<Val> {
                Ok(Val::$t(self))
            }

            #[doc(hidden)]
            #[inline]
            fn into_slot(self) -> GResult<Slot> {
                Ok(Slot::$t(self.into_raw()))
            }
        }

        impl<'a> IntoVal for &'a Root<$t> {
            #[inline]
            fn into_val(self) -> GResult<Val> {
                Ok(Val::$t((*self).clone()))
            }

            #[doc(hidden)]
            #[inline]
            fn into_slot(self) -> GResult<Slot> {
                Ok(Slot::$t((*self).to_raw()))
            }
        }

        impl<'a> IntoVal for &'a mut Root<$t> {
            #[inline]
            fn into_val(self) -> GResult<Val> {
                Ok(Val::$t((*self).clone()))
            }

            #[doc(hidden)]
            #[inline]
            fn into_slot(self) -> GResult<Slot> {
                Ok(Slot::$t((*self).to_raw()))
            }
        }

        impl IntoVal for Raw<$t> {
            #[inline]
            fn into_val(self) -> GResult<Val> {
                Ok(Val::$t(self.into_root()))
            }

            #[doc(hidden)]
            #[inline]
            fn into_slot(self) -> GResult<Slot> {
                Ok(Slot::$t(self))
            }
        }
    };
}

impl_into_val_root!(Arr);
impl_into_val_root!(Str);
impl_into_val_root!(Tab);
impl_into_val_root!(GIter);
impl_into_val_root!(Obj);
impl_into_val_root!(Class);
impl_into_val_root!(GFn);
impl_into_val_root!(Coro);
impl_into_val_root!(RData);
impl_into_val_root!(RFn);

impl<T> IntoVal for RRoot<T> {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::RData(self.into_root()))
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        Ok(Slot::RData(self.into_raw()))
    }
}

impl<'a, T> IntoVal for &'a RRoot<T> {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::RData((*self).to_root()))
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        Ok(Slot::RData((*self).to_raw()))
    }
}

impl<'a, T> IntoVal for &'a mut RRoot<T> {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::RData((*self).to_root()))
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        Ok(Slot::RData((*self).to_raw()))
    }
}

impl IntoVal for Deque {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        match self {
            Deque::Arr(root) => Ok(Val::Arr(root)),
            Deque::Str(root) => Ok(Val::Str(root)),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        match self {
            Deque::Arr(root) => Ok(Slot::Arr(root.into_raw())),
            Deque::Str(root) => Ok(Slot::Str(root.into_raw())),
        }
    }
}

impl IntoVal for Callable {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        match self {
            Callable::GFn(root) => Ok(Val::GFn(root)),
            Callable::RFn(root) => Ok(Val::RFn(root)),
            Callable::Class(root) => Ok(Val::Class(root)),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        match self {
            Callable::GFn(root) => Ok(Slot::GFn(root.into_raw())),
            Callable::RFn(root) => Ok(Slot::RFn(root.into_raw())),
            Callable::Class(root) => Ok(Slot::Class(root.into_raw())),
        }
    }
}

impl IntoVal for Expander {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        match self {
            Expander::GFn(root) => Ok(Val::GFn(root)),
            Expander::RFn(root) => Ok(Val::RFn(root)),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        match self {
            Expander::GFn(root) => Ok(Slot::GFn(root.into_raw())),
            Expander::RFn(root) => Ok(Slot::RFn(root.into_raw())),
        }
    }
}

impl IntoVal for Iterable {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        match self {
            Iterable::Arr(root) => Ok(Val::Arr(root)),
            Iterable::Str(root) => Ok(Val::Str(root)),
            Iterable::Tab(root) => Ok(Val::Tab(root)),
            Iterable::GIter(root) => Ok(Val::GIter(root)),
            Iterable::Coro(root) => Ok(Val::Coro(root)),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        match self {
            Iterable::Arr(root) => Ok(Slot::Arr(root.into_raw())),
            Iterable::Str(root) => Ok(Slot::Str(root.into_raw())),
            Iterable::Tab(root) => Ok(Slot::Tab(root.into_raw())),
            Iterable::GIter(root) => Ok(Slot::GIter(root.into_raw())),
            Iterable::Coro(root) => Ok(Slot::Coro(root.into_raw())),
        }
    }
}

impl IntoVal for GIterLen {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        match self {
            GIterLen::Exact(len) => Ok(Val::Int(len as i32)),
            GIterLen::Infinite => Ok(Val::Sym(INFINITE_SYM)),
            GIterLen::Unknown => Ok(Val::Sym(UNKNOWN_SYM)),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        match self {
            GIterLen::Exact(len) => Ok(Slot::Int(len as i32)),
            GIterLen::Infinite => Ok(Slot::Sym(INFINITE_SYM)),
            GIterLen::Unknown => Ok(Slot::Sym(UNKNOWN_SYM)),
        }
    }
}

impl IntoVal for Ordering {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        match self {
            Ordering::Less => Ok(Val::Sym(LT_SYM)),
            Ordering::Equal => Ok(Val::Sym(NUM_EQ_SYM)),
            Ordering::Greater => Ok(Val::Sym(GT_SYM)),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        match self {
            Ordering::Less => Ok(Slot::Sym(LT_SYM)),
            Ordering::Equal => Ok(Slot::Sym(NUM_EQ_SYM)),
            Ordering::Greater => Ok(Slot::Sym(GT_SYM)),
        }
    }
}

macro_rules! impl_refs_to_clone_types {
    ($($t:ty),+) => (
        $(
            impl<'a> IntoVal for &'a $t {
                #[inline]
                fn into_val(self) -> GResult<Val> {
                    (*self).clone().into_val()
                }

                #[doc(hidden)]
                #[inline]
                fn into_slot(self) -> GResult<Slot> {
                    (*self).clone().into_slot()
                }
            }

            impl<'a> IntoVal for &'a mut $t {
                #[inline]
                fn into_val(self) -> GResult<Val> {
                    (*self).clone().into_val()
                }

                #[doc(hidden)]
                #[inline]
                fn into_slot(self) -> GResult<Slot> {
                    (*self).clone().into_slot()
                }
            }
        )+
    );
}

impl_refs_to_clone_types!(Deque, Callable, Expander, Iterable, GIterLen, Ordering);

macro_rules! impl_into_val_bounded_int {
    ($self_type:ty) => {
        impl IntoVal for $self_type {
            #[inline]
            fn into_val(self) -> GResult<Val> {
                if let Ok(converted) = self.try_into() {
                    Ok(Val::Int(converted))
                } else {
                    bail!(
                        "the result was {}, which is outside the range of an i32",
                        self
                    )
                }
            }

            #[doc(hidden)]
            #[inline]
            fn into_slot(self) -> GResult<Slot> {
                if let Ok(converted) = self.try_into() {
                    Ok(Slot::Int(converted))
                } else {
                    bail!(
                        "the result was {}, which is outside the range of an i32",
                        self
                    )
                }
            }
        }
    };
}

impl_into_val_bounded_int!(i64);
impl_into_val_bounded_int!(i128);
impl_into_val_bounded_int!(isize);
impl_into_val_bounded_int!(u32);
impl_into_val_bounded_int!(u64);
impl_into_val_bounded_int!(u128);
impl_into_val_bounded_int!(usize);

impl IntoVal for f64 {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Flo(self as f32))
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        Ok(Slot::Flo(self as f32))
    }
}

impl IntoVal for Num {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        match self {
            Num::Int(i) => Ok(Val::Int(i)),
            Num::Flo(f) => Ok(Val::Flo(f)),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn into_slot(self) -> GResult<Slot> {
        match self {
            Num::Int(i) => Ok(Slot::Int(i)),
            Num::Flo(f) => Ok(Slot::Flo(f)),
        }
    }
}

impl<T: IntoVal> IntoVal for Vec<T> {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(self)?))
    }
}

impl<'a, T> IntoVal for &'a Vec<T>
where
    &'a T: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(self)?))
    }
}

impl<'a, T> IntoVal for &'a mut Vec<T>
where
    &'a mut T: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(self)?))
    }
}

impl<T: IntoVal> IntoVal for VecDeque<T> {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(self)?))
    }
}

impl<'a, T> IntoVal for &'a VecDeque<T>
where
    &'a T: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(self)?))
    }
}

impl<'a, T> IntoVal for &'a mut VecDeque<T>
where
    &'a mut T: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(self)?))
    }
}

impl<A: smallvec::Array> IntoVal for SmallVec<A>
where
    A::Item: IntoVal,
{
    #[inline]
    fn into_val(mut self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(self.drain(..))?))
    }
}

impl<'a, A: smallvec::Array> IntoVal for &'a SmallVec<A>
where
    &'a A::Item: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(self)?))
    }
}

impl<'a, A: smallvec::Array> IntoVal for &'a mut SmallVec<A>
where
    &'a mut A::Item: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(self)?))
    }
}

impl<'a, T> IntoVal for &'a [T]
where
    &'a T: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(self)?))
    }
}

impl<'a, T> IntoVal for &'a mut [T]
where
    &'a mut T: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(self)?))
    }
}

impl<T, const N: usize> IntoVal for [T; N]
where
    for<'a> &'a T: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(&self[..])?))
    }
}

impl<'a, T, const N: usize> IntoVal for &'a [T; N]
where
    &'a T: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(&self[..])?))
    }
}

impl<'a, T, const N: usize> IntoVal for &'a mut [T; N]
where
    &'a mut T: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Arr(glsp::arr_from_iter(&mut self[..])?))
    }
}

macro_rules! impl_into_val_tuple {
    ($len:literal: $($t:ident $i:tt),+) => (
        impl<$($t),+> IntoVal for ($($t,)+)
        where
            $( $t: IntoVal ),+
        {
            #[inline]
            fn into_val(self) -> GResult<Val> {
                let arr = glsp::arr_with_capacity($len);

                $(
                    arr.push(self.$i)?;
                )+

                Ok(Val::Arr(arr))
            }
        }

        impl<'a, $($t),+> IntoVal for &'a ($($t,)+)
        where
            $( &'a $t: IntoVal ),+
        {
            #[inline]
            fn into_val(self) -> GResult<Val> {
                let arr = glsp::arr_with_capacity($len);

                $(
                    arr.push(&self.$i)?;
                )+

                Ok(Val::Arr(arr))
            }
        }

        impl<'a, $($t),+> IntoVal for &'a mut ($($t,)+)
        where
            $( &'a mut $t: IntoVal ),+
        {
            #[inline]
            fn into_val(self) -> GResult<Val> {
                let arr = glsp::arr_with_capacity($len);

                $(
                    arr.push(&mut self.$i)?;
                )+

                Ok(Val::Arr(arr))
            }
        }
    );
}

impl_into_val_tuple!( 1: A 0);
impl_into_val_tuple!( 2: A 0, B 1);
impl_into_val_tuple!( 3: A 0, B 1, C 2);
impl_into_val_tuple!( 4: A 0, B 1, C 2, D 3);
impl_into_val_tuple!( 5: A 0, B 1, C 2, D 3, E 4);
impl_into_val_tuple!( 6: A 0, B 1, C 2, D 3, E 4, F 5);
impl_into_val_tuple!( 7: A 0, B 1, C 2, D 3, E 4, F 5, G 6);
impl_into_val_tuple!( 8: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);
impl_into_val_tuple!( 9: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8);
impl_into_val_tuple!(10: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9);
impl_into_val_tuple!(11: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10);
impl_into_val_tuple!(12: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11);

impl IntoVal for String {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Str(glsp::str_from_rust_str(&self)))
    }
}

impl<'a> IntoVal for &'a String {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Str(glsp::str_from_rust_str(self)))
    }
}

impl<'a> IntoVal for &'a mut String {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Str(glsp::str_from_rust_str(self)))
    }
}

impl<'a> IntoVal for &'a str {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Str(glsp::str_from_rust_str(self)))
    }
}

impl<'a> IntoVal for &'a mut str {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Str(glsp::str_from_rust_str(self)))
    }
}

impl IntoVal for CString {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (&self as &CStr).into_val()
    }
}

impl<'a> IntoVal for &'a CString {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (self as &CStr).into_val()
    }
}

impl<'a> IntoVal for &'a mut CString {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (self as &CStr).into_val()
    }
}

impl<'a> IntoVal for &'a CStr {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        match self.to_str() {
            Ok(str_ref) => str_ref.into_val(),
            Err(_) => bail!("CStr contained non-UTF-8 data"),
        }
    }
}

impl<'a> IntoVal for &'a mut CStr {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (self as &CStr).into_val()
    }
}

impl IntoVal for OsString {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (&self as &OsStr).into_val()
    }
}

impl<'a> IntoVal for &'a OsString {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (self as &OsStr).into_val()
    }
}

impl<'a> IntoVal for &'a mut OsString {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (self as &OsStr).into_val()
    }
}

impl<'a> IntoVal for &'a OsStr {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        match self.to_str() {
            Some(str_ref) => str_ref.into_val(),
            None => bail!("OsStr contained non-UTF-8 data"),
        }
    }
}

impl<'a> IntoVal for &'a mut OsStr {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (self as &OsStr).into_val()
    }
}

impl IntoVal for PathBuf {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (&self as &Path).into_val()
    }
}

impl<'a> IntoVal for &'a PathBuf {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (&self as &Path).into_val()
    }
}

impl<'a> IntoVal for &'a mut PathBuf {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (&self as &Path).into_val()
    }
}

impl<'a> IntoVal for &'a Path {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        self.as_os_str().into_val()
    }
}

impl<'a> IntoVal for &'a mut Path {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        (self as &Path).into_val()
    }
}

impl<K: IntoVal, V: IntoVal, S> IntoVal for HashMap<K, V, S> {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Tab(glsp::tab_from_iter(self)?))
    }
}

impl<'a, K, V, S> IntoVal for &'a HashMap<K, V, S>
where
    &'a K: IntoVal,
    &'a V: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Tab(glsp::tab_from_iter(self)?))
    }
}

impl<'a, K, V, S> IntoVal for &'a mut HashMap<K, V, S>
where
    &'a K: IntoVal,
    &'a mut V: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Tab(glsp::tab_from_iter(self)?))
    }
}

impl<K: IntoVal, V: IntoVal> IntoVal for BTreeMap<K, V> {
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Tab(glsp::tab_from_iter(self)?))
    }
}

impl<'a, K, V> IntoVal for &'a BTreeMap<K, V>
where
    &'a K: IntoVal,
    &'a V: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Tab(glsp::tab_from_iter(self)?))
    }
}

impl<'a, K, V> IntoVal for &'a mut BTreeMap<K, V>
where
    &'a K: IntoVal,
    &'a mut V: IntoVal,
{
    #[inline]
    fn into_val(self) -> GResult<Val> {
        Ok(Val::Tab(glsp::tab_from_iter(self)?))
    }
}

//-------------------------------------------------------------------------------------------------
// FromVal implementations
//-------------------------------------------------------------------------------------------------

impl FromVal for Val {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        Ok(val.clone())
    }

    #[doc(hidden)]
    #[inline]
    fn from_slot(slot: &Slot) -> GResult<Self> {
        Ok(slot.root())
    }
}

impl FromVal for Slot {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        Ok(Slot::from_val(val))
    }

    #[doc(hidden)]
    #[inline]
    fn from_slot(slot: &Slot) -> GResult<Self> {
        Ok(slot.clone())
    }
}

macro_rules! impl_from_val_infallible(
    ($(($t:ty, $variant:ident)),+) => (
        $(
            impl FromVal for $t {
                #[inline]
                fn from_val(val: &Val) -> GResult<Self> {
                    match *val {
                        Val::$variant(interior) => Ok(interior as $t),
                        ref val => bail!("expected {}, received {}",
                                         stringify!($t), val.a_type_name())
                    }
                }

                #[doc(hidden)]
                #[inline]
                fn from_slot(slot: &Slot) -> GResult<Self> {
                    match *slot {
                        Slot::$variant(interior) => Ok(interior as $t),
                        ref slot => bail!("expected {}, received {}",
                                          stringify!($t), slot.a_type_name())
                    }
                }
            }
        )+
    );
);

impl_from_val_infallible!(
    (i32, Int),
    (i64, Int),
    (i128, Int),
    (isize, Int),
    (char, Char),
    (bool, Bool),
    (Sym, Sym)
);

macro_rules! impl_from_val_root(
    ($(($t:ty, $variant:ident)),+) => (
        $(
            impl FromVal for Root<$t> {
                #[inline]
                fn from_val(val: &Val) -> GResult<Self> {
                    match *val {
                        Val::$variant(ref root) => Ok(root.clone()),
                        ref val => bail!("expected {}, received {}",
                                         stringify!(Root<$t>), val.a_type_name())
                    }
                }

                #[doc(hidden)]
                #[inline]
                fn from_slot(slot: &Slot) -> GResult<Self> {
                    match *slot {
                        Slot::$variant(ref raw) => Ok(raw.root()),
                        ref slot => bail!("expected {}, received {}",
                                          stringify!(Root<$t>), slot.a_type_name())
                    }
                }
            }

            impl FromVal for Raw<$t> {
                #[inline]
                fn from_val(val: &Val) -> GResult<Self> {
                    match *val {
                        Val::$variant(ref root) => Ok(root.as_raw().clone()),
                        ref val => bail!("expected {}, received {}",
                                         stringify!(Raw<$t>), val.a_type_name())
                    }
                }

                #[doc(hidden)]
                #[inline]
                fn from_slot(slot: &Slot) -> GResult<Self> {
                    match *slot {
                        Slot::$variant(ref raw) => Ok(raw.clone()),
                        ref slot => bail!("expected {}, received {}",
                                          stringify!(Raw<$t>), slot.a_type_name())
                    }
                }
            }
        )+
    );
);

impl_from_val_root!(
    (Arr, Arr),
    (Str, Str),
    (Tab, Tab),
    (GIter, GIter),
    (Obj, Obj),
    (GFn, GFn),
    (Class, Class),
    (Coro, Coro),
    (RData, RData),
    (RFn, RFn)
);

impl<T: StaticMarker> FromVal for RRoot<T> {
    #[inline]
    fn from_val(val: &Val) -> GResult<RRoot<T>> {
        match val {
            Val::RData(root) => Ok(RRoot::new(root.clone())),
            val => bail!(
                "expected RRoot<{}>, received {}",
                type_name::<T>(),
                val.a_type_name()
            ),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn from_slot(slot: &Slot) -> GResult<RRoot<T>> {
        match slot {
            Slot::RData(raw) => Ok(RRoot::new(raw.root())),
            val => bail!(
                "expected RRoot<{}>, received {}",
                type_name::<T>(),
                val.a_type_name()
            ),
        }
    }
}

macro_rules! impl_from_val_int_fallible_small(
    ($($t:ident),+) => (
        $(
            impl FromVal for $t {
                #[inline]
                fn from_val(val: &Val) -> GResult<Self> {
                    match *val {
                        Val::Int(i) if i >= $t::MIN as i32 && i <= $t::MAX as i32 => {
                            Ok(i as $t)
                        }
                        Val::Int(i) => {
                            bail!("expected {}, received an int with value {}",
                                  stringify!($t), i)
                        }
                        ref val => bail!("expected {}, received {}",
                                         stringify!($t), val.a_type_name())
                    }
                }

                #[doc(hidden)]
                #[inline]
                fn from_slot(slot: &Slot) -> GResult<Self> {
                    match *slot {
                        Slot::Int(i) if i >= $t::MIN as i32 && i <= $t::MAX as i32 => {
                            Ok(i as $t)
                        }
                        Slot::Int(i) => {
                            bail!("expected {}, received an int with value {}",
                                  stringify!($t), i)
                        }
                        ref slot => bail!("expected {}, received {}",
                                          stringify!($t), slot.a_type_name())
                    }
                }
            }
        )+
    );
);

impl_from_val_int_fallible_small!(i8, i16, u8, u16);

macro_rules! impl_from_val_int_fallible_large(
    ($($t:ty),+) => (
        $(
            impl FromVal for $t {
                #[inline]
                fn from_val(val: &Val) -> GResult<Self> {
                    match *val {
                        Val::Int(i) if i >= 0 => {
                            Ok(i as $t)
                        }
                        Val::Int(i) => {
                            bail!("expected {}, received an int with value {}",
                                  stringify!($t), i)
                        }
                        ref val => bail!("expected {}, received {}",
                                         stringify!($t), val.a_type_name())
                    }
                }

                #[doc(hidden)]
                #[inline]
                fn from_slot(slot: &Slot) -> GResult<Self> {
                    match *slot {
                        Slot::Int(i) if i >= 0 => {
                            Ok(i as $t)
                        }
                        Slot::Int(i) => {
                            bail!("expected {}, received an int with value {}",
                                  stringify!($t), i)
                        }
                        ref slot => bail!("expected {}, received {}",
                                          stringify!($t), slot.a_type_name())
                    }
                }
            }
        )+
    );
);

impl_from_val_int_fallible_large!(u32, u64, u128, usize);

impl FromVal for f32 {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Flo(f) => Ok(f),
            ref val => bail!("expected f32, received {}", val.a_type_name()),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn from_slot(slot: &Slot) -> GResult<Self> {
        match *slot {
            Slot::Flo(f) => Ok(f),
            ref slot => bail!("expected f32, received {}", slot.a_type_name()),
        }
    }
}

impl FromVal for f64 {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Flo(f) => Ok(f as f64),
            ref val => bail!("expected f64, received {}", val.a_type_name()),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn from_slot(slot: &Slot) -> GResult<Self> {
        match *slot {
            Slot::Flo(f) => Ok(f as f64),
            ref slot => bail!("expected f64, received {}", slot.a_type_name()),
        }
    }
}

impl FromVal for Num {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Int(i) => Ok(Num::Int(i)),
            Val::Flo(f) => Ok(Num::Flo(f)),
            ref val => bail!("expected Num, received {}", val.a_type_name()),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn from_slot(slot: &Slot) -> GResult<Self> {
        match *slot {
            Slot::Int(i) => Ok(Num::Int(i)),
            Slot::Flo(f) => Ok(Num::Flo(f)),
            ref slot => bail!("expected Num, received {}", slot.a_type_name()),
        }
    }
}

impl FromVal for Deque {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Arr(ref root) => Ok(Deque::Arr(root.clone())),
            Val::Str(ref root) => Ok(Deque::Str(root.clone())),
            ref val => bail!("expected Deque, received {}", val.a_type_name()),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn from_slot(slot: &Slot) -> GResult<Self> {
        match *slot {
            Slot::Arr(ref raw) => Ok(Deque::Arr(raw.root())),
            Slot::Str(ref raw) => Ok(Deque::Str(raw.root())),
            ref slot => bail!("expected Deque, received {}", slot.a_type_name()),
        }
    }
}

impl FromVal for Callable {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::GFn(ref root) => Ok(Callable::GFn(root.clone())),
            Val::RFn(ref root) => Ok(Callable::RFn(root.clone())),
            Val::Class(ref root) => Ok(Callable::Class(root.clone())),
            ref val => bail!("expected Callable, received {}", val.a_type_name()),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn from_slot(slot: &Slot) -> GResult<Self> {
        match *slot {
            Slot::GFn(ref raw) => Ok(Callable::GFn(raw.root())),
            Slot::RFn(ref raw) => Ok(Callable::RFn(raw.root())),
            Slot::Class(ref raw) => Ok(Callable::Class(raw.root())),
            ref slot => bail!("expected Callable, received {}", slot.a_type_name()),
        }
    }
}

impl FromVal for Expander {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::GFn(ref root) => Ok(Expander::GFn(root.clone())),
            Val::RFn(ref root) => Ok(Expander::RFn(root.clone())),
            ref val => bail!("expected Expander, received {}", val.a_type_name()),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn from_slot(slot: &Slot) -> GResult<Self> {
        match *slot {
            Slot::GFn(ref raw) => Ok(Expander::GFn(raw.root())),
            Slot::RFn(ref raw) => Ok(Expander::RFn(raw.root())),
            ref slot => bail!("expected Expander, received {}", slot.a_type_name()),
        }
    }
}

impl FromVal for Iterable {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match val {
            Val::Arr(root) => Ok(Iterable::Arr(root.clone())),
            Val::Str(root) => Ok(Iterable::Str(root.clone())),
            Val::Tab(root) => Ok(Iterable::Tab(root.clone())),
            Val::GIter(root) => Ok(Iterable::GIter(root.clone())),
            Val::Coro(root) => Ok(Iterable::Coro(root.clone())),
            val => bail!("expected Iterable, received {}", val.a_type_name()),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn from_slot(slot: &Slot) -> GResult<Self> {
        match slot {
            Slot::Arr(raw) => Ok(Iterable::Arr(raw.root())),
            Slot::Str(raw) => Ok(Iterable::Str(raw.root())),
            Slot::Tab(raw) => Ok(Iterable::Tab(raw.root())),
            Slot::GIter(raw) => Ok(Iterable::GIter(raw.root())),
            Slot::Coro(raw) => Ok(Iterable::Coro(raw.root())),
            slot => bail!("expected Iterable, received {}", slot.a_type_name()),
        }
    }
}

impl FromVal for EnvMode {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Sym(sym) => match sym {
                FRESH_SYM => Ok(EnvMode::Fresh),
                COPIED_SYM => Ok(EnvMode::Copied),
                _ => bail!("expected an EnvMode, received the symbol {}", sym),
            },
            ref val => bail!("expected an EnvMode, received {}", val.a_type_name()),
        }
    }
}

impl FromVal for Ordering {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Sym(LT_SYM) => Ok(Ordering::Less),
            Val::Sym(NUM_EQ_SYM) => Ok(Ordering::Equal),
            Val::Sym(GT_SYM) => Ok(Ordering::Greater),
            ref val => bail!("expected Ordering, received {}", val.a_type_name()),
        }
    }

    #[doc(hidden)]
    #[inline]
    fn from_slot(slot: &Slot) -> GResult<Self> {
        match *slot {
            Slot::Sym(LT_SYM) => Ok(Ordering::Less),
            Slot::Sym(NUM_EQ_SYM) => Ok(Ordering::Equal),
            Slot::Sym(GT_SYM) => Ok(Ordering::Greater),
            ref slot => bail!("expected Ordering, received {}", slot.a_type_name()),
        }
    }
}

impl<T: FromVal> FromVal for Vec<T> {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Arr(ref arr) => {
                let mut vec = Vec::<T>::with_capacity(arr.len());

                let arr_borrow = arr.borrow();
                for slot in arr_borrow.iter() {
                    vec.push(T::from_slot(slot)?);
                }

                Ok(vec)
            }
            ref val => bail!("expected a Vec, received {}", val.a_type_name()),
        }
    }
}

impl<T: FromVal> FromVal for VecDeque<T> {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Arr(ref arr) => {
                let mut vec = VecDeque::<T>::with_capacity(arr.len());

                let arr_borrow = arr.borrow();
                for slot in arr_borrow.iter() {
                    vec.push_back(T::from_slot(slot)?);
                }

                Ok(vec)
            }
            ref val => bail!("expected a VecDeque, received {}", val.a_type_name()),
        }
    }
}

impl<A> FromVal for SmallVec<A>
where
    A: smallvec::Array + StaticMarker,
    A::Item: FromVal,
{
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Arr(ref arr) => {
                let mut small_vec = SmallVec::<A>::with_capacity(arr.len());

                let arr_borrow = arr.borrow();
                for slot in arr_borrow.iter() {
                    small_vec.push(A::Item::from_slot(slot)?);
                }

                Ok(small_vec)
            }
            ref val => bail!("expected a SmallVec, received {}", val.a_type_name()),
        }
    }
}

impl<T: FromVal, const N: usize> FromVal for [T; N] {
    #[inline]
    fn from_val(val: &Val) -> GResult<[T; N]> {
        match *val {
            Val::Arr(ref arr) => {
                ensure!(
                    arr.len() == N,
                    "expected a [T; {}], received an array of length {}",
                    N,
                    arr.len()
                );

                //todo: this is wildly inefficient; improve it once better ways to construct
                //non-Copy const generic arrays are available. maybe SmallVec?
                let mut vals = Vec::<T>::with_capacity(N);
                for i in 0..N {
                    vals.push(arr.get::<T>(i)?);
                }

                Ok(TryFrom::try_from(vals).ok().unwrap())
            }
            ref val => {
                bail!("expected a [T; {}], received {}", N, val.a_type_name())
            }
        }
    }
}

macro_rules! impl_from_val_tuple {
    ($len:literal: $($t:ident $i:tt),+) => (
        impl<$($t),+> FromVal for ($($t,)+)
        where
            $($t: FromVal),+
        {
            #[inline]
            fn from_val(val: &Val) -> GResult<($($t,)+)> {
                match *val {
                    Val::Arr(ref arr) => {
                        ensure!(arr.len() == $len,
                                "expected a {}-element tuple, received an arr of length {}",
                                $len, arr.len());

                        Ok(($(
                            arr.get::<$t>($i)?,
                        )*))
                    }
                    ref val => bail!("expected a tuple, received {}", val.a_type_name())
                }
            }
        }
    );
}

impl_from_val_tuple!( 1: A 0);
impl_from_val_tuple!( 2: A 0, B 1);
impl_from_val_tuple!( 3: A 0, B 1, C 2);
impl_from_val_tuple!( 4: A 0, B 1, C 2, D 3);
impl_from_val_tuple!( 5: A 0, B 1, C 2, D 3, E 4);
impl_from_val_tuple!( 6: A 0, B 1, C 2, D 3, E 4, F 5);
impl_from_val_tuple!( 7: A 0, B 1, C 2, D 3, E 4, F 5, G 6);
impl_from_val_tuple!( 8: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);
impl_from_val_tuple!( 9: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8);
impl_from_val_tuple!(10: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9);
impl_from_val_tuple!(11: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10);
impl_from_val_tuple!(12: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11);

impl FromVal for String {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Str(ref st) => Ok(st.to_string()),
            ref val => bail!("expected a str, received {}", val.a_type_name()),
        }
    }
}

impl FromVal for CString {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Str(ref st) => match CString::new(st.to_string()) {
                Ok(cstring) => Ok(cstring),
                Err(_) => {
                    bail!("expected a C string, received a str with an inner nul")
                }
            },
            ref val => bail!("expected a C string, received {}", val.a_type_name()),
        }
    }
}

impl FromVal for PathBuf {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Str(ref st) => Ok(PathBuf::from(st.to_string())),
            ref val => bail!("expected a path, received {}", val.a_type_name()),
        }
    }
}

impl FromVal for OsString {
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Str(ref st) => Ok(OsString::from(st.to_string())),
            ref val => bail!("expected an OS string, received {}", val.a_type_name()),
        }
    }
}

impl<K, V, S> FromVal for HashMap<K, V, S>
where
    K: HashEqMarker + FromVal + StaticMarker,
    V: FromVal + StaticMarker,
    S: BuildHasherDefaultMarker + StaticMarker,
{
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Tab(ref tab) => {
                let s = S::default();
                let mut hash_map = HashMap::<K, V, S>::with_capacity_and_hasher(tab.len(), s);

                let tab_borrow = tab.borrow();
                for (internal_key, internal_value) in tab_borrow.iter() {
                    let key = K::from_slot(internal_key)?;
                    let value = V::from_slot(internal_value)?;

                    if hash_map.insert(key, value).is_some() {
                        bail!("duplicate key in HashMap argument");
                    }
                }

                Ok(hash_map)
            }
            ref val => bail!("expected a HashMap, received {}", val.a_type_name()),
        }
    }
}

// BTreeMap<K, V>
//-----------------------------------------------------------------------------

impl<K, V> FromVal for BTreeMap<K, V>
where
    K: OrdMarker + FromVal + StaticMarker,
    V: FromVal + StaticMarker,
{
    #[inline]
    fn from_val(val: &Val) -> GResult<Self> {
        match *val {
            Val::Tab(ref tab) => {
                let mut btree_map = BTreeMap::<K, V>::new();

                let tab_borrow = tab.borrow();
                for (internal_key, internal_value) in tab_borrow.iter() {
                    let key = K::from_slot(internal_key)?;
                    let value = V::from_slot(internal_value)?;

                    if btree_map.insert(key, value).is_some() {
                        bail!("duplicate key in BTreeMap argument");
                    }
                }

                Ok(btree_map)
            }
            ref val => bail!("expected a BTreeMap, received {}", val.a_type_name()),
        }
    }
}

//-------------------------------------------------------------------------------------------------
// FromArg, FromArgRef
//-------------------------------------------------------------------------------------------------

#[doc(hidden)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ArgType {
    RGlobal,
    Normal,
    Option,
    Rest,
}

#[doc(hidden)]
#[rustc_specialization_trait]
pub trait FromArg: Sized {
    type Temp: 'static;
    type OutputCtor: ?Sized + for<'a> Ctor<'a>;

    fn arg_type() -> ArgType;
    fn make_temp(args: &[Slot], i: usize) -> GResult<Self::Temp>;
    fn from_arg<'a>(temp: &'a mut Self::Temp) -> GResult<<Self::OutputCtor as Ctor<'a>>::Ty>;
}

#[doc(hidden)]
#[rustc_specialization_trait]
pub trait FromArgRef: Sized {
    type Temp: 'static;
    type OutputCtor: for<'a> Ctor<'a>;

    fn arg_type() -> ArgType;
    fn make_temp(args: &[Slot], i: usize) -> GResult<Self::Temp>;
    fn from_arg<'a>(temp: &'a mut Self::Temp) -> GResult<<Self::OutputCtor as Ctor<'a>>::Ty>;
}

/*
The Ctor trait is a workaround for rust's current lack of GAT support. it should
eventually be replaced with a generic lifetime on FromArgRef::OutputCtor (todo)
*/

#[doc(hidden)]
pub trait Ctor<'a> {
    type Ty;
}

#[doc(hidden)]
pub struct ValCtor<T>(PhantomData<T>);

impl<'a, T> Ctor<'a> for ValCtor<T> {
    type Ty = T;
}

#[doc(hidden)]
pub struct OptionCtor<T: ?Sized>(PhantomData<T>);

impl<'a, T: Ctor<'a> + ?Sized> Ctor<'a> for OptionCtor<T> {
    type Ty = Option<T::Ty>;
}

#[doc(hidden)]
pub struct RestCtor<T>(PhantomData<T>);

impl<'a, T: 'a> Ctor<'a> for RestCtor<T> {
    type Ty = Rest<'a, T>;
}

#[doc(hidden)]
pub struct RefCtor<T: ?Sized>(PhantomData<T>);

impl<'a, T: ?Sized + 'a> Ctor<'a> for RefCtor<T> {
    type Ty = &'a T;
}

#[doc(hidden)]
pub struct RefMutCtor<T: ?Sized>(PhantomData<T>);

impl<'a, T: ?Sized + 'a> Ctor<'a> for RefMutCtor<T> {
    type Ty = &'a mut T;
}

impl<T: FromVal> FromArg for T {
    type Temp = Slot;
    type OutputCtor = ValCtor<T>;

    #[inline]
    fn arg_type() -> ArgType {
        ArgType::Normal
    }

    #[inline]
    fn make_temp(args: &[Slot], i: usize) -> GResult<Slot> {
        Ok(args[i].clone())
    }

    #[inline]
    fn from_arg(temp: &mut Slot) -> GResult<T> {
        T::from_slot(temp)
    }
}

impl<T: FromArg> FromArg for Option<T> {
    type Temp = Option<T::Temp>;
    type OutputCtor = OptionCtor<<T as FromArg>::OutputCtor>;

    #[inline]
    fn arg_type() -> ArgType {
        ArgType::Option
    }

    #[inline]
    fn make_temp(args: &[Slot], i: usize) -> GResult<Option<T::Temp>> {
        if i >= args.len() || matches!(args[i], Slot::Nil) {
            Ok(None)
        } else {
            Ok(Some(T::make_temp(args, i)?))
        }
    }

    #[inline]
    fn from_arg(
        temp: &mut Option<T::Temp>,
    ) -> GResult<<<Self as FromArg>::OutputCtor as Ctor>::Ty> {
        match temp {
            None => Ok(None),
            Some(temp) => Ok(Some(T::from_arg(temp)?)),
        }
    }
}

/**
An adapter type which collects any number of trailing function arguments.

When [binding a Rust function](fn.rfn.html) so that it can be called from GameLisp, if `Rest<T>`
appears at the end of the function's parameter list, it will collect all of the function's trailing
arguments, converting them into a temporary array of `T` which is usually stored on the stack.

`Rest<T>` can be dereferenced to a mutable slice, `[T]`. It's also iterable, yielding `T`, `&T`
or `&mut T` as appropriate.

```
# extern crate glsp_engine as glsp;
# use glsp::*;
# 
# Engine::new().run(|| {
//a function which adds one or more integers together
fn add_integers(first: i32, rest: Rest<i32>) -> i32 {
    let mut accumulator = first;
    for integer in rest {
        accumulator += integer;
    }

    accumulator
}

//bind this function to a global variable
glsp::bind_rfn("add-integers", &add_integers)?;

//the function can now be called from GameLisp
glsp::load_str("
    (add-integers 42)
    (add-integers 10 20 30 40 50)
")?;

//or from Rust
Rest::with([20, 30, 40, 50].iter().copied(), |rest| {
    add_integers(10, rest);
});
# Ok(()) }).unwrap();
```

It's possible to construct a custom `Rest<T>` yourself by calling [`Rest::with`](#method.with),
but it's usually not elegant. Instead, consider defining a Rust function which receives a slice
or a generic `IntoIterator`, and a wrapper which receives a `Rest<T>` and forwards it to the
original function.

```
# extern crate glsp_engine as glsp;
# use glsp::*;
# 
# Engine::new().run(|| {
# 
fn add_integers(first: i32, rest: &[i32]) -> i32 {
    rest.iter().fold(first, |a, b| a + *b)
}

glsp::bind_rfn("add_integers", &|first: i32, rest: Rest<i32>| -> i32 {
    add_integers(first, &*rest)
})?;
# 
# Ok(()) }).unwrap();
```
*/
pub struct Rest<'a, T>(&'a mut Option<SmallVec<[T; 8]>>);

impl<'a, T> Rest<'a, T> {
    #[inline]
    pub fn with<S, F, R>(src: S, f: F) -> R
    where
        S: IntoIterator<Item = T>,
        F: FnOnce(Rest<T>) -> R,
    {
        f(Rest(&mut Some(src.into_iter().collect())))
    }
}

impl<'a, T> Deref for Rest<'a, T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        self.0.as_ref().unwrap()
    }
}

impl<'a, T> DerefMut for Rest<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.0.as_mut().unwrap()
    }
}

impl<'a, T, I: SliceIndex<[T]>> Index<I> for Rest<'a, T> {
    type Output = I::Output;

    #[inline]
    fn index(&self, index: I) -> &I::Output {
        &(**self)[index]
    }
}

impl<'a, T, I: SliceIndex<[T]>> IndexMut<I> for Rest<'a, T> {
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut I::Output {
        &mut (&mut **self)[index]
    }
}

impl<'a, T> IntoIterator for Rest<'a, T> {
    type Item = T;
    type IntoIter = smallvec::IntoIter<[T; 8]>;

    #[inline]
    fn into_iter(self) -> smallvec::IntoIter<[T; 8]> {
        self.0.take().unwrap().into_iter()
    }
}

impl<'r, 'a: 'r, T> IntoIterator for &'r Rest<'a, T> {
    type Item = &'r T;
    type IntoIter = slice::Iter<'r, T>;

    #[inline]
    fn into_iter(self) -> slice::Iter<'r, T> {
        self.0.as_ref().unwrap().iter()
    }
}

impl<'r, 'a: 'r, T> IntoIterator for &'r mut Rest<'a, T> {
    type Item = &'r mut T;
    type IntoIter = slice::IterMut<'r, T>;

    #[inline]
    fn into_iter(self) -> slice::IterMut<'r, T> {
        self.0.as_mut().unwrap().iter_mut()
    }
}

impl<'r, T: FromVal> FromArg for Rest<'r, T> {
    type Temp = (SmallVec<[Slot; 8]>, Option<SmallVec<[T; 8]>>);
    type OutputCtor = RestCtor<T>;

    #[inline]
    fn arg_type() -> ArgType {
        ArgType::Rest
    }

    #[inline]
    fn make_temp(
        args: &[Slot],
        i: usize,
    ) -> GResult<(SmallVec<[Slot; 8]>, Option<SmallVec<[T; 8]>>)> {
        /*
        we can't just call T::from_slot() here, because the argument slice
        is borrowed. a user-defined from_val() could do something which
        pushes to the reg stack, causing a panic
        */

        Ok((
            args[min(i, args.len())..].iter().cloned().collect(),
            Some(SmallVec::with_capacity(args.len().saturating_sub(i))),
        ))
    }

    #[inline]
    fn from_arg<'a>(
        temp: &'a mut (SmallVec<[Slot; 8]>, Option<SmallVec<[T; 8]>>),
    ) -> GResult<Rest<'a, T>> {
        for arg in &temp.0 {
            temp.1.as_mut().unwrap().push(T::from_slot(arg)?);
        }

        Ok(Rest(&mut temp.1))
    }
}

impl<'r, T: FromVal> FromArg for &'r [T] {
    type Temp = (Slot, SmallVec<[T; 8]>);
    type OutputCtor = RefCtor<[T]>;

    #[inline]
    fn arg_type() -> ArgType {
        ArgType::Normal
    }

    #[inline]
    fn make_temp(args: &[Slot], i: usize) -> GResult<(Slot, SmallVec<[T; 8]>)> {
        /*
        we can't just call T::from_slot() here, because the argument slice
        is borrowed. a user-defined from_val() could do something which
        pushes to the reg stack, causing a panic
        */

        Ok((args[i].clone(), SmallVec::with_capacity(args.len() - i)))
    }

    #[inline]
    fn from_arg<'a>(temp: &'a mut (Slot, SmallVec<[T; 8]>)) -> GResult<&'a [T]> {
        temp.1 = SmallVec::from_slot(&temp.0)?;
        Ok(&temp.1)
    }
}

impl<'r> FromArg for &'r str {
    type Temp = SmallVec<[u8; 128]>;
    type OutputCtor = RefCtor<str>;

    #[inline]
    fn arg_type() -> ArgType {
        ArgType::Normal
    }

    #[inline]
    fn make_temp(args: &[Slot], i: usize) -> GResult<SmallVec<[u8; 128]>> {
        let mut vec = SmallVec::<[u8; 128]>::new();

        match &args[i] {
            Slot::Str(st) => write!(&mut vec, "{}", st).unwrap(),
            slot => bail!("expected a &str, received {}", slot.a_type_name()),
        }

        Ok(vec)
    }

    #[inline]
    fn from_arg<'a>(temp: &'a mut SmallVec<[u8; 128]>) -> GResult<&'a str> {
        Ok(str::from_utf8(&temp[..]).unwrap())
    }
}

macro_rules! impl_from_arg_text_slice (
    ($(($slice_type:ident, $owned_type:ident)),+) => (
        $(
            impl<'r> FromArg for &'r $slice_type {
                type Temp = $owned_type;
                type OutputCtor = RefCtor<$slice_type>;

                #[inline]
                fn arg_type() -> ArgType {
                    ArgType::Normal
                }

                #[inline]
                fn make_temp(args: &[Slot], i: usize) -> GResult<$owned_type> {
                    $owned_type::from_slot(&args[i])
                }

                #[inline]
                fn from_arg<'a>(temp: &'a mut $owned_type) -> GResult<&'a $slice_type> {
                    Ok(&**temp)
                }
            }
        )+
    );
);

impl_from_arg_text_slice!((Path, PathBuf), (CStr, CString), (OsStr, OsString));

/*
rustc doesn't yet support specialization of associated types, so we need to dispatch this
dynamically instead. i suspect that we won't see any unnecessary memcpys/memsets when,
for example, a DynTemp::<SmallVec<[u8; 1024]>>::RRef is constructed; it'll just waste a
bit of uninitialized stack space instead.

i also hope that the `match` branches will be consistently optimized out, but i'm less
confident of that...

we hedge our bets by marking make_temp() as #[inline(always)] rather than #[inline]
*/

pub enum DynTemp<T: 'static> {
    RGlobalRef(RGlobalRef<T>),
    RRef(RRef<T>),
    Slot(Slot),
}

pub enum DynTempMut<T: 'static> {
    RGlobalRefMut(RGlobalRefMut<T>),
    RRefMut(RRefMut<T>),
}

impl<'r, T: StaticMarker> FromArgRef for &'r T {
    type Temp = DynTemp<T>;
    type OutputCtor = RefCtor<T>;

    #[inline]
    default fn arg_type() -> ArgType {
        ArgType::Normal
    }

    #[inline(always)]
    default fn make_temp(args: &[Slot], i: usize) -> GResult<DynTemp<T>> {
        match &args[i] {
            Slot::RData(rdata) => Ok(DynTemp::RRef(rdata.borrow())),
            slot => bail!(
                "expected &{}, received {}",
                type_name::<T>(),
                slot.a_type_name()
            ),
        }
    }

    #[inline]
    default fn from_arg<'a>(temp: &'a mut DynTemp<T>) -> GResult<&'a T> {
        match temp {
            DynTemp::RRef(temp) => Ok(&**temp),
            _ => unreachable!(),
        }
    }
}

impl<'r, T: StaticMarker> FromArgRef for &'r mut T {
    type Temp = DynTempMut<T>;
    type OutputCtor = RefMutCtor<T>;

    #[inline]
    default fn arg_type() -> ArgType {
        ArgType::Normal
    }

    #[inline(always)]
    default fn make_temp(args: &[Slot], i: usize) -> GResult<DynTempMut<T>> {
        match &args[i] {
            Slot::RData(rdata) => Ok(DynTempMut::RRefMut(rdata.borrow_mut())),
            slot => bail!(
                "expected &mut {}, received {}",
                type_name::<T>(),
                slot.a_type_name()
            ),
        }
    }

    #[inline]
    default fn from_arg<'a>(temp: &'a mut DynTempMut<T>) -> GResult<&'a mut T> {
        match temp {
            DynTempMut::RRefMut(temp) => Ok(&mut **temp),
            _ => unreachable!(),
        }
    }
}

/*
ideally, we would impl FromArgRef for &mut T: FromVal + IntoVal. this would bring us closer
to supporting more Rust function signatures verbatim (e.g. File::read_to_string takes a
&mut String), although it would carry a counterintuitive performance cost in some cases.

however, there's no way to establish a hierarchial relationship between those two traits
and RGlobal.

we could exploit the FromArg/FromArgRef dichotomy by implementing FromArg for
&mut T: FromVal + IntoVal, because users are unlikely to implement both RGlobal and
FromVal/IntoVal for the same type. however, this conflicts with the blanket IntoVal
implementation for all 'static types.
*/

impl<'r, T: RGlobalMarker + Sized + StaticMarker> FromArgRef for &'r T {
    #[inline]
    fn arg_type() -> ArgType {
        ArgType::RGlobal
    }

    #[inline(always)]
    fn make_temp(_args: &[Slot], _i: usize) -> GResult<DynTemp<T>> {
        Ok(DynTemp::RGlobalRef(glsp::try_rglobal::<T>()?))
    }

    #[inline]
    fn from_arg<'a>(temp: &'a mut DynTemp<T>) -> GResult<&'a T> {
        match temp {
            DynTemp::RGlobalRef(temp) => Ok(&**temp),
            _ => unreachable!(),
        }
    }
}

impl<'r, T: RGlobalMarker + Sized + StaticMarker> FromArgRef for &'r mut T {
    #[inline]
    fn arg_type() -> ArgType {
        ArgType::RGlobal
    }

    #[inline(always)]
    fn make_temp(_args: &[Slot], _i: usize) -> GResult<DynTempMut<T>> {
        Ok(DynTempMut::RGlobalRefMut(glsp::try_rglobal_mut::<T>()?))
    }

    #[inline]
    fn from_arg<'a>(temp: &'a mut DynTempMut<T>) -> GResult<&'a mut T> {
        match temp {
            DynTempMut::RGlobalRefMut(temp) => Ok(&mut **temp),
            _ => unreachable!(),
        }
    }
}

impl<T: FromArgRef> FromArgRef for Option<T> {
    type Temp = Option<T::Temp>;
    type OutputCtor = OptionCtor<<T as FromArgRef>::OutputCtor>;

    #[inline]
    fn arg_type() -> ArgType {
        ArgType::Option
    }

    #[inline]
    fn make_temp(args: &[Slot], i: usize) -> GResult<Option<T::Temp>> {
        if i >= args.len() || matches!(args[i], Slot::Nil) {
            Ok(None)
        } else {
            Ok(Some(T::make_temp(args, i)?))
        }
    }

    #[inline]
    fn from_arg<'a>(
        temp: &'a mut Option<T::Temp>,
    ) -> GResult<<<Self as FromArgRef>::OutputCtor as Ctor>::Ty> {
        match temp {
            None => Ok(None),
            Some(temp) => Ok(Some(T::from_arg(temp)?)),
        }
    }
}

macro_rules! impl_pointee_from_arg_ref {
    ($($pointee:ident),+) => (
        $(
            impl<'r> FromArgRef for &'r $pointee {
                #[inline(always)]
                fn make_temp(args: &[Slot], i: usize) -> GResult<DynTemp<$pointee>> {
                    Ok(DynTemp::Slot(args[i].clone()))
                }

                #[inline]
                fn from_arg<'a>(temp: &'a mut DynTemp<$pointee>) -> GResult<&'a $pointee> {
                    match temp {
                        DynTemp::Slot(Slot::$pointee(ref raw)) => Ok(&**raw),
                        DynTemp::Slot(val) => {
                            bail!(
                                "expected &{}, received {}",
                                stringify!($pointee), (val.type_name())
                            )
                        }
                        _ => unreachable!()
                    }
                }
            }
        )+
    );
}

impl_pointee_from_arg_ref!(Arr, Str, Tab, GIter, GFn, Obj, Class, Coro, RData, RFn);

//-------------------------------------------------------------------------------------------------
// wrap() and its supporting traits
//-------------------------------------------------------------------------------------------------

#[doc(hidden)]
pub trait CalculateArgLimits {
    fn calculate_arg_limits() -> (usize, usize)
    where
        Self: Sized;
}

#[doc(hidden)]
pub trait WrappedCall: CalculateArgLimits {
    fn arg_limits(&self) -> (usize, usize);
    fn wrapped_call(&self, args: Ref<[Slot]>) -> GResult<Slot>;
}

#[doc(hidden)]
pub struct Wrapper<ArgsWithTag, Ret, F> {
    f: F,
    arg_limits: (usize, usize),
    phantom: PhantomData<(ArgsWithTag, Ret)>,
}

/*
previously, we enforced that F, and the wrapper thunk itself, must be convertible to bare
function pointers. however, this was difficult to enforce and carried almost no performance
benefit - benchmarking suggests that indirecting a call through Box<dyn WrappedCall> only costs
one additional cpu cycle, compared to a bare function pointer.
*/

pub(crate) fn wrap<ArgsWithTag, Ret, F>(f: F) -> Box<dyn WrappedCall>
where
    Wrapper<ArgsWithTag, Ret, F>: WrappedCall + 'static,
{
    Box::new(Wrapper {
        f,
        arg_limits: Wrapper::<ArgsWithTag, Ret, F>::calculate_arg_limits(),
        phantom: PhantomData,
    })
}

macro_rules! arg_limits_fn {
    ($fn_name:ident, $arg_count: literal; $($i:literal)*) => (

        #[allow(dead_code, unused_assignments, unused_mut, unused_variables)]
        const fn $fn_name(args: [ArgType; $arg_count]) -> Result<(usize, usize), &'static str> {
            let mut required_args = 0;
            let mut optional_args = 0;
            let mut seen_rest = false;
            let mut seen_opt = false;

           $(
                match args[$i] {
                    ArgType::Normal => {
                        if seen_rest {
                            return Err("Rest<T> followed by a normal argument")
                        }

                        required_args += optional_args;
                        optional_args = 0;

                        required_args += 1;
                    }
                    ArgType::Option => {
                         if seen_rest {
                            return Err("Rest<T> followed by an Option<T> argument")
                        }

                        optional_args += 1;
                    }
                    ArgType::Rest => {
                        seen_rest = true;
                    }
                    _ => () //unreachable, but we can't panic in const fns yet (todo)
                }
            )*

            Ok((
                required_args,
                if seen_rest { usize::MAX } else { required_args + optional_args }
            ))
        }
    );
}

arg_limits_fn!(arg_limits_0, 0;);
arg_limits_fn!(arg_limits_1, 1; 0);
arg_limits_fn!(arg_limits_2, 2; 0 1);
arg_limits_fn!(arg_limits_3, 3; 0 1 2);
arg_limits_fn!(arg_limits_4, 4; 0 1 2 3);
arg_limits_fn!(arg_limits_5, 5; 0 1 2 3 4);
arg_limits_fn!(arg_limits_6, 6; 0 1 2 3 4 5);
arg_limits_fn!(arg_limits_7, 7; 0 1 2 3 4 5 6);
arg_limits_fn!(arg_limits_8, 8; 0 1 2 3 4 5 6 7);

#[doc(hidden)]
pub struct TagArg;

#[doc(hidden)]
pub struct TagArgRef;

//an ugly workaround for rustc's tendency to fail to normalize associated types
//when they're accessed via a HRTB
pub trait OutputIntoVal<T>: Fn<T> {
    fn output_into_slot(output: <Self as FnOnce<T>>::Output) -> GResult<Slot>;
}
impl<T, F> OutputIntoVal<T> for F
where
    F: Fn<T>,
    <F as FnOnce<T>>::Output: IntoVal,
{
    fn output_into_slot(output: <Self as FnOnce<T>>::Output) -> GResult<Slot> {
        output.into_slot()
    }
}

macro_rules! wrap_tuple_impls {
    (
        $arg_limits_fn:ident, $arg_count:literal;
        $($arg_t:ident $arg_trait:ident $arg_tag:ident),*;
        $($temp_name:ident),*
    ) => (

        impl<$($arg_t,)* Ret, F> /*const*/
            CalculateArgLimits
            for
            Wrapper<($(($arg_t, $arg_tag),)*), Ret, F>
        where
            $(
                $arg_t: $arg_trait,
            )*
        {
            fn calculate_arg_limits() -> (usize, usize) where Self: Sized {
                $arg_limits_fn([$($arg_t::arg_type(),)*]).unwrap()
            }
        }

        #[allow(dead_code, unused_assignments, unused_mut, unused_variables)]
        impl<$($arg_t,)* Ret, F>
            WrappedCall
            for
            Wrapper<($(($arg_t, $arg_tag),)*), Ret, F>
        where
            $(
                $arg_t: $arg_trait,
            )*
            Ret: IntoVal,
            F: Fn($($arg_t,)*) -> Ret,
            F: for<'a> Fn(
                $(<<$arg_t as $arg_trait>::OutputCtor as Ctor<'a>>::Ty,)*
            ) -> Ret,
            F: for<'a> OutputIntoVal<(
                $(<<$arg_t as $arg_trait>::OutputCtor as Ctor<'a>>::Ty,)*
            )>
        {
            fn arg_limits(&self) -> (usize, usize) {
                self.arg_limits
            }

            fn wrapped_call(&self, args: Ref<[Slot]>) -> GResult<Slot> {
                /*
                a dilemma: should we emit argument bounds-checks here, or store them
                in the RFn struct and check them before performing the call?

                advantages of checking here:
                - will probably increase bound-check elision in make_temp() and from_arg(),
                  iff the limits are constants
                - if we can make them fully const, comparisons against `usize` constants will
                  probably be a cycle or two cheaper than comparisons against fields

                disadvantages:
                - if we can't make the arg_limits an actual const, constant-folding might
                  fail, executing a large amount of code on every call
                - we could move these checks elsewhere to reduce monomorphization cost (presumably
                  only slightly?) and the initial cost of compiling the macro-generated impls

                for now, we perform the check here, but it's non-const. intend to switch the
                arg limits into constants asap, once the const_trait_impls feature no longer has
                the incomplete_features warning (todo)
                */

                if args.len() < self.arg_limits.0 {
                    bail!(
                        "too few arguments: received {}, expected at least {}",
                        args.len(),
                        self.arg_limits.0
                    )
                }

                if args.len() > self.arg_limits.1 {
                    bail!(
                        "too many arguments: received {}, expected no more than {}",
                        args.len(),
                        self.arg_limits.1
                    )
                }

                let mut arg_i = 0;

                $(
                    let mut $temp_name = $arg_t::make_temp(&args, arg_i)?;

                    if $arg_t::arg_type() != ArgType::RGlobal {
                        arg_i += 1;
                    }
                )*

                drop(args);

                let output = (self.f)($(
                    $arg_t::from_arg(&mut $temp_name)?
                ),*);

                F::output_into_slot(output)
            }
        }
    );
}

/*
for processing arguments, we really need three blanket implementations: FromArg for all
T: FromVal, FromArg for all &T where T: 'static, and FromArg for all &T where
T: RGlobal + 'static. unfortunately, this isn't even possible with specialization, because
rustc doesn't consider &T to be "more specialized" than T, and there's no way to prevent
the user from implementing FromVal for reference types.

we can achieve what we need by having two non-overlapping traits, one for reference arguments
and one for value arguments. however, rustc's trait system doesn't support any kind of
negative reasoning, and it might never be supported, because it causes serious trouble with api
stability. this means that we have no good way of saying "these two traits are disjoint, please
fail at [compile time|runtime] if they're both implemented on the same type".

our very ugly trick to work around it...

    trait Api<T> { ... }

    struct TagA;
    struct TagB;

    impl<T: TraitA> Api<(T, TagA)> for T { ... }
    impl<T: TraitB> Api<(T, TagB)> for T { ... }

    fn api<T, F>(f: F) where F: Api<T> + Fn(T) { ... }

Api<(T, TagA)> and Api<(T, TagB)> don't overlap; they're distinct traits which happen to
have a similar api, and similar behaviour for the purposes of type inference.

if the user passes in a function to api() with an argument which implements both TraitA and
TraitB, type inference will fail; but for any argument which implements only one of those
two traits, inference will succeed. we're careful to only implement FromArg and FromArgRef in
ways that won't overlap, unless the user implements FromVal for something very weird like
a static slice.

if Api<T>'s type parameter is replaced with a tuple of 2-tuples, this lets us blanket-implement
for<T: TraitA, U: TraitB> Api<((T, TagA), (U, TagB))>, and then blanket-implement
for<T: TraitB, U: TraitA> Api<((T, TagB), (U, TagA))>, and so on, so that we can support
arbitrary argument lists.

we can infer for any combination of up to N arguments by using macros to implement the trait
((N+1)^2 - 1) times. we currently support eight arguments, which requires 511 implementations.
rustc handles it better than you might expect; it costs around 20 seconds on an incremental
`cargo check` when modifying the `glsp-engine` crate, and it seems to have zero cost on
an incremental `cargo check` for a crate which uses glsp, when glsp itself hasn't been modified.

based on testing, switching from nested tuples ((A, B), (C, D)) to flat tuples (A, B, C, D)
doesn't seem to affect compile time for `glsp-stdlib`. reducing the maximum number of arguments
from 8 down to 4 also had no effect.
*/

macro_rules! forward_to_wrap_tuple_impls {
    (
        $($t:ident $i:literal $temp_name:ident $trait:ident $tag:ident,)*;
        $arg_count:literal $arg_limits_fn:ident
    ) => (
        wrap_tuple_impls!(
            $arg_limits_fn, $arg_count;
            $($t $trait $tag),*;
            $($temp_name),*
        );
    );
}

macro_rules! recurse {
    (
        $first_t:ident $first_i:literal $first_temp:ident $first_fn:ident,
        $($rest_t:ident $rest_i:literal $rest_temp:ident $rest_fn:ident,)*;
        $($accum_t:ident $accum_i:literal $accum_temp:ident
            $accum_fn:ident $accum_trait:ident $accum_tag:ident,)*
    ) => (
        forward_to_wrap_tuple_impls!(
            $($accum_t $accum_i $accum_temp $accum_trait $accum_tag,)*; $first_i $first_fn
        );

        recurse!(
            $($rest_t $rest_i $rest_temp $rest_fn,)*;
            $($accum_t $accum_i $accum_temp $accum_fn $accum_trait $accum_tag,)*
            $first_t $first_i $first_temp $first_fn FromArg TagArg,
        );
        recurse!(
            $($rest_t $rest_i $rest_temp $rest_fn,)*;
            $($accum_t $accum_i $accum_temp $accum_fn $accum_trait $accum_tag,)*
            $first_t $first_i $first_temp $first_fn FromArgRef TagArgRef,
        );
    );
    (; $($t:ident $i:literal $temp:ident $arg_limits_fn:ident $trait:ident $tag:ident,)*) => (
        forward_to_wrap_tuple_impls!(
            $($t $i $temp $trait $tag,)*; 8 arg_limits_8
        );
    );
}

recurse!(
    T0 0 temp_0 arg_limits_0,
    T1 1 temp_1 arg_limits_1,
    T2 2 temp_2 arg_limits_2,
    T3 3 temp_3 arg_limits_3,
    T4 4 temp_4 arg_limits_4,
    T5 5 temp_5 arg_limits_5,
    T6 6 temp_6 arg_limits_6,
    T7 7 temp_7 arg_limits_7,
    ;
);

//-------------------------------------------------------------------------------------------------
// Callable, CallableOps, IntoCallArgs
//-------------------------------------------------------------------------------------------------

/**
A type-erased `callable`.

Because this type implements the [`CallableOps` trait](trait.CallableOps.html), you can call
it directly, without needing to access the underlying types.
*/

#[derive(Clone, Debug)]
pub enum Callable {
    RFn(Root<RFn>),
    GFn(Root<GFn>),
    Class(Root<Class>),
}

/**
The `callable` abstract type.

[`glsp:call`](fn.call.html) can be used to call any type which implements this trait.

This trait is [sealed]. It's not possible to implement this trait for your own types.

[sealed]: https://rust-lang.github.io/api-guidelines/future-proofing.html#sealed-traits-protect-against-downstream-implementations-c-sealed
*/

pub trait CallableOps: callable_ops_private::Sealed {
    #[doc(hidden)]
    fn receive_call(&self, arg_count: usize) -> GResult<Val>;

    ///Returns this function's registered name, if any.
    fn name(&self) -> Option<Sym>;

    ///Returns this function's minimum and maximum argument count.
    fn arg_limits(&self) -> (usize, Option<usize>);

    ///Returns this function's minimum argument count.
    fn min_args(&self) -> usize {
        self.arg_limits().0
    }

    ///Returns this function's maximum argument count, if any.
    fn max_args(&self) -> Option<usize> {
        self.arg_limits().1
    }
}

mod callable_ops_private {
    use crate::{
        class::Class,
        code::GFn,
        engine::RFn,
        gc::{Raw, Root},
        wrap::Callable,
    };

    pub trait Sealed {}

    impl Sealed for Callable {}
    impl Sealed for Root<RFn> {}
    impl Sealed for Raw<RFn> {}
    impl Sealed for Root<Class> {}
    impl Sealed for Raw<Class> {}
    impl Sealed for Root<GFn> {}
    impl Sealed for Raw<GFn> {}
}

impl CallableOps for Callable {
    #[inline]
    fn receive_call(&self, arg_count: usize) -> GResult<Val> {
        match *self {
            Callable::RFn(ref rfn_root) => {
                glsp::call_rfn(rfn_root, arg_count).map(|slot| slot.root())
            }
            Callable::GFn(ref gfn_root) => glsp::call_gfn(gfn_root, arg_count),
            Callable::Class(ref class_root) => {
                Ok(Val::Obj(glsp::call_class(class_root, arg_count)?))
            }
        }
    }

    fn arg_limits(&self) -> (usize, Option<usize>) {
        match *self {
            Callable::RFn(ref rfn_root) => rfn_root.arg_limits(),
            Callable::GFn(ref gfn_root) => gfn_root.arg_limits(),
            Callable::Class(ref class_root) => class_root.arg_limits(),
        }
    }

    fn name(&self) -> Option<Sym> {
        match *self {
            Callable::RFn(ref rfn_root) => rfn_root.name(),
            Callable::GFn(ref gfn_root) => gfn_root.name(),
            Callable::Class(ref class_root) => class_root.name(),
        }
    }
}

/**
A type which can be converted into the arguments to a function call.

It's not possible to implement this trait for your own types, but it's implemented for tuples,
slices, arrays, and references to the same, when their elements all implement
[`IntoVal`](trait.IntoVal.html).

Functions like [`glsp:call`](fn.call.html) and [`Obj::call`](struct.Obj.html#method.call) are
generic over this trait:

```
# extern crate glsp_engine as glsp;
# use glsp::*;
# 
# Engine::new().run(|| {
# let my_arr = arr![];
# glsp::bind_global("push!", glsp::rfn(&|_arr: Root<Arr>, _val: Val| { }))?;
# 
let push_rfn: Root<RFn> = glsp::global("push!")?;
let _: Val = glsp::call(&push_rfn, (my_arr, 100i32))?;
# 
# Ok(()) }).unwrap();
```

Due to some limitations in Rust's type system, argument lists can be slightly awkward:

- It's not possible to pass in `&[]` or `[]` to represent an empty argument list. You should
  use `&()` or `()` instead.
- `IntoVal` isn't implemented for references to references, so types like `&(&f32, &f32)`,
  `&[&Root<Arr>]` and `[&i32; 16]` won't be accepted. When working with references, prefer
  to use tuples, like `(&f32, &f32)`.
*/

pub trait IntoCallArgs: into_call_args_private::Sealed {
    fn arg_count(&self) -> usize;
    fn into_call_args<E: Extend<Slot>>(self, dst: &mut E) -> GResult<()>;
}

mod into_call_args_private {
    use crate::wrap::{IntoVal, Rest};

    pub trait Sealed {}

    impl<'a, T> Sealed for &'a [T] where &'a T: IntoVal {}
    impl<'a, T> Sealed for &'a mut [T] where &'a mut T: IntoVal {}

    impl<T, const N: usize> Sealed for [T; N] where for<'a> &'a T: IntoVal {}
    impl<'a, T, const N: usize> Sealed for &'a [T; N] where &'a T: IntoVal {}
    impl<'a, T, const N: usize> Sealed for &'a mut [T; N] where &'a mut T: IntoVal {}

    impl<'a, T> Sealed for Rest<'a, T> where T: IntoVal {}
    impl<'r, 'a: 'r, T> Sealed for &'r Rest<'a, T> where &'r T: IntoVal {}
    impl<'r, 'a: 'r, T> Sealed for &'r mut Rest<'a, T> where &'r mut T: IntoVal {}

    impl Sealed for () {}
    impl<'a> Sealed for &'a () {}
    impl<'a> Sealed for &'a mut () {}
}

impl<'a, T> IntoCallArgs for &'a [T]
where
    &'a T: IntoVal,
{
    fn arg_count(&self) -> usize {
        self.len()
    }

    fn into_call_args<E: Extend<Slot>>(self, dst: &mut E) -> GResult<()> {
        let mut result = Ok(());
        dst.extend(self.iter().map(|item| match item.into_slot() {
            Ok(slot) => slot,
            Err(err) => {
                if result.is_ok() {
                    result = Err(err);
                }
                Slot::Nil
            }
        }));
        result
    }
}

impl<'a, T> IntoCallArgs for &'a mut [T]
where
    &'a mut T: IntoVal,
{
    fn arg_count(&self) -> usize {
        self.len()
    }

    fn into_call_args<E: Extend<Slot>>(self, dst: &mut E) -> GResult<()> {
        let mut result = Ok(());
        dst.extend(self.iter_mut().map(|item| match item.into_slot() {
            Ok(slot) => slot,
            Err(err) => {
                if result.is_ok() {
                    result = Err(err);
                }
                Slot::Nil
            }
        }));
        result
    }
}

impl<T, const N: usize> IntoCallArgs for [T; N]
where
    for<'a> &'a T: IntoVal,
{
    fn arg_count(&self) -> usize {
        N
    }

    fn into_call_args<E: Extend<Slot>>(self, dst: &mut E) -> GResult<()> {
        (&self[..]).into_call_args(dst)
    }
}

impl<'a, T, const N: usize> IntoCallArgs for &'a [T; N]
where
    &'a T: IntoVal,
{
    fn arg_count(&self) -> usize {
        N
    }

    fn into_call_args<E: Extend<Slot>>(self, dst: &mut E) -> GResult<()> {
        (&self[..]).into_call_args(dst)
    }
}

impl<'a, T, const N: usize> IntoCallArgs for &'a mut [T; N]
where
    &'a mut T: IntoVal,
{
    fn arg_count(&self) -> usize {
        N
    }

    fn into_call_args<E: Extend<Slot>>(self, dst: &mut E) -> GResult<()> {
        (&mut self[..]).into_call_args(dst)
    }
}

impl<'a, T> IntoCallArgs for Rest<'a, T>
where
    T: IntoVal,
{
    fn arg_count(&self) -> usize {
        self.len()
    }

    fn into_call_args<E: Extend<Slot>>(self, dst: &mut E) -> GResult<()> {
        let mut result = Ok(());
        dst.extend(self.into_iter().map(|item| match item.into_slot() {
            Ok(slot) => slot,
            Err(err) => {
                if result.is_ok() {
                    result = Err(err);
                }
                Slot::Nil
            }
        }));
        result
    }
}

impl<'r, 'a: 'r, T> IntoCallArgs for &'r Rest<'a, T>
where
    &'r T: IntoVal,
{
    fn arg_count(&self) -> usize {
        self.len()
    }

    fn into_call_args<E: Extend<Slot>>(self, dst: &mut E) -> GResult<()> {
        let mut result = Ok(());
        dst.extend(self.into_iter().map(|item| match item.into_slot() {
            Ok(slot) => slot,
            Err(err) => {
                if result.is_ok() {
                    result = Err(err);
                }
                Slot::Nil
            }
        }));
        result
    }
}

impl<'r, 'a: 'r, T> IntoCallArgs for &'r mut Rest<'a, T>
where
    &'r mut T: IntoVal,
{
    fn arg_count(&self) -> usize {
        self.len()
    }

    fn into_call_args<E: Extend<Slot>>(self, dst: &mut E) -> GResult<()> {
        let mut result = Ok(());
        dst.extend(self.into_iter().map(|item| match item.into_slot() {
            Ok(slot) => slot,
            Err(err) => {
                if result.is_ok() {
                    result = Err(err);
                }
                Slot::Nil
            }
        }));
        result
    }
}

impl IntoCallArgs for () {
    fn arg_count(&self) -> usize {
        0
    }

    fn into_call_args<E: Extend<Slot>>(self, _dst: &mut E) -> GResult<()> {
        Ok(())
    }
}

impl<'a> IntoCallArgs for &'a () {
    fn arg_count(&self) -> usize {
        0
    }

    fn into_call_args<E: Extend<Slot>>(self, _dst: &mut E) -> GResult<()> {
        Ok(())
    }
}

impl<'a> IntoCallArgs for &'a mut () {
    fn arg_count(&self) -> usize {
        0
    }

    fn into_call_args<E: Extend<Slot>>(self, _dst: &mut E) -> GResult<()> {
        Ok(())
    }
}

macro_rules! impl_into_call_args_tuple {
    ($len:literal: $($t:ident $i:tt),+) => (
        impl<$($t),+> into_call_args_private::Sealed for ($($t,)+)
            where $( $t: IntoVal ),+ { }

        impl<'a, $($t),+> into_call_args_private::Sealed for &'a ($($t,)+)
            where $( &'a $t: IntoVal ),+ { }

        impl<'a, $($t),+> into_call_args_private::Sealed for &'a mut ($($t,)+)
            where $( &'a mut $t: IntoVal ),+ { }

        impl<$($t),+> IntoCallArgs for ($($t,)+)
        where
            $( $t: IntoVal ),+
        {
            fn arg_count(&self) -> usize {
                $len
            }

            fn into_call_args<EE: Extend<Slot>>(self, dst: &mut EE) -> GResult<()> {
                let slots = [ $(
                    (self.$i).into_slot()?
                ),+ ];

                dst.extend(slots.iter().cloned());
                Ok(())
            }
        }

        impl<'a, $($t),+> IntoCallArgs for &'a ($($t,)+)
        where
            $( &'a $t: IntoVal ),+
        {
            fn arg_count(&self) -> usize {
                $len
            }

            fn into_call_args<EE: Extend<Slot>>(self, dst: &mut EE) -> GResult<()> {
                let slots = [ $(
                    (&self.$i).into_slot()?
                ),+ ];

                dst.extend(slots.iter().cloned());
                Ok(())
            }
        }

        impl<'a, $($t),+> IntoCallArgs for &'a mut ($($t,)+)
        where
            $( &'a mut $t: IntoVal ),+
        {
            fn arg_count(&self) -> usize {
                $len
            }

            fn into_call_args<EE: Extend<Slot>>(self, dst: &mut EE) -> GResult<()> {
                let slots = [ $(
                    (&mut self.$i).into_slot()?
                ),+ ];

                dst.extend(slots.iter().cloned());
                Ok(())
            }
        }
    );
}

impl_into_call_args_tuple!( 1: A 0);
impl_into_call_args_tuple!( 2: A 0, B 1);
impl_into_call_args_tuple!( 3: A 0, B 1, C 2);
impl_into_call_args_tuple!( 4: A 0, B 1, C 2, D 3);
impl_into_call_args_tuple!( 5: A 0, B 1, C 2, D 3, E 4);
impl_into_call_args_tuple!( 6: A 0, B 1, C 2, D 3, E 4, F 5);
impl_into_call_args_tuple!( 7: A 0, B 1, C 2, D 3, E 4, F 5, G 6);
impl_into_call_args_tuple!( 8: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);
impl_into_call_args_tuple!( 9: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8);
impl_into_call_args_tuple!(10: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9);
impl_into_call_args_tuple!(11: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10);
impl_into_call_args_tuple!(12: A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11);
