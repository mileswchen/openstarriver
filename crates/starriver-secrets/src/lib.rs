//! File-backed credentials + secret reference resolution (`credentials`
//! / `credentials-local` parity): `~/.starriver/.credentials.yaml` holds a
//! versioned document, and `api-key` style values resolve through layered
//! references (managed store wins over legacy `secrets.json`, env vars last).
pub mod credentials;
pub mod secrets;
