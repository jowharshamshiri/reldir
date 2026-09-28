//! Analyses of what the data says, shared by every command that proposes
//! schema: inference declares what they find, lint reports it, and doctor
//! applies it. One analysis, so the three can never disagree about what a
//! folder of JSON means.

pub mod references;
