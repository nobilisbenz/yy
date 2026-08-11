//! Interned atoms.
//!
//! Interning costs a round trip each, so do them all once at connect time
//! rather than on the summon path — the whole point of this crate is that
//! showing the dock costs no round trips it can avoid.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, ConnectionExt as _};

macro_rules! atoms {
    ($($field:ident => $name:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy)]
        #[allow(non_snake_case)]
        pub struct Atoms {
            $(pub $field: Atom,)+
        }

        impl Atoms {
            // Fields and locals keep the exact EWMH spelling. Renaming them to
            // snake_case would make every property here harder to check
            // against the spec, which is the only reference that matters.
            #[allow(non_snake_case)]
            pub fn intern(conn: &impl Connection) -> Result<Self, crate::X11Error> {
                // Fire every request before blocking on any reply.
                $(let $field = conn.intern_atom(false, $name)?;)+
                Ok(Self {
                    $($field: $field.reply()?.atom,)+
                })
            }
        }
    };
}

atoms! {
    _NET_ACTIVE_WINDOW      => b"_NET_ACTIVE_WINDOW",
    _NET_CLIENT_LIST        => b"_NET_CLIENT_LIST",
    _NET_CURRENT_DESKTOP    => b"_NET_CURRENT_DESKTOP",
    _NET_WM_DESKTOP         => b"_NET_WM_DESKTOP",
    _NET_WM_NAME            => b"_NET_WM_NAME",
    _NET_WM_PID             => b"_NET_WM_PID",
    _NET_WM_STATE           => b"_NET_WM_STATE",
    _NET_WM_STATE_ABOVE     => b"_NET_WM_STATE_ABOVE",
    _NET_WM_STATE_SKIP_PAGER   => b"_NET_WM_STATE_SKIP_PAGER",
    _NET_WM_STATE_SKIP_TASKBAR => b"_NET_WM_STATE_SKIP_TASKBAR",
    _NET_WM_STATE_STICKY    => b"_NET_WM_STATE_STICKY",
    _NET_WM_STRUT           => b"_NET_WM_STRUT",
    _NET_WM_STRUT_PARTIAL   => b"_NET_WM_STRUT_PARTIAL",
    _NET_WM_WINDOW_TYPE     => b"_NET_WM_WINDOW_TYPE",
    _NET_WM_WINDOW_TYPE_UTILITY => b"_NET_WM_WINDOW_TYPE_UTILITY",
    _NET_WORKAREA           => b"_NET_WORKAREA",
    _MOTIF_WM_HINTS         => b"_MOTIF_WM_HINTS",
    UTF8_STRING             => b"UTF8_STRING",
}
