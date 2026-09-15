//! Print the `--disallowedTools` argument the README recommends.
//!
//! An example rather than a binary: this crate is a library, and the one thing
//! outside it that needs the list is CI, checking the README has not drifted
//! from the map. See `ccn_core::disallowed_tools_arg`.

fn main() {
    println!("{}", ccn_core::disallowed_tools_arg());
}
