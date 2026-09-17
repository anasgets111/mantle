//! `xdg_shell`'s client-picked roles: `window` (`xdg_toplevel`) with its size negotiation, and
//! `popup` (`xdg_popup`) with positioners and the ADR-0049/0051 dismissal latch. Shared
//! bind/paint/(un)map logic is in `surface`.

use super::*;
mod popup;
mod window;
