//! Хелперы, чтобы хендлер писал `?` и `.spec()`, а не разворачивал руками.

use crate::errors::{Aux, Generic};

pub trait SpecExt: Sized {
    /// Своя ошибка операции → `Generic::Spec`.
    fn spec<S: From<Self>>(self) -> Generic<S> {
        Generic::Spec(S::from(self))
    }
}

impl<T> SpecExt for T {}

pub trait ResultExt: Sized {
    type Ok;
    type Err;

    fn aux_err(self) -> Result<Self::Ok, Aux>
    where
        Self::Err: Into<Aux>;
}

impl<O, E> ResultExt for Result<O, E> {
    type Ok = O;
    type Err = E;

    #[track_caller]
    fn aux_err(self) -> Result<O, Aux>
    where
        E: Into<Aux>,
    {
        self.map_err(Into::into)
    }
}

pub trait BoolExt: Sized {
    fn true_or<E>(self, err: E) -> Result<(), E>;
    fn false_or<E>(self, err: E) -> Result<(), E>;
}

impl BoolExt for bool {
    fn true_or<E>(self, err: E) -> Result<(), E> {
        if self { Ok(()) } else { Err(err) }
    }

    fn false_or<E>(self, err: E) -> Result<(), E> {
        if self { Err(err) } else { Ok(()) }
    }
}

pub trait OptExt<T>: Sized {
    fn none_or<E>(self, err: E) -> Result<T, E>;
}

impl<T> OptExt<T> for Option<T> {
    fn none_or<E>(self, err: E) -> Result<T, E> {
        self.ok_or(err)
    }
}
