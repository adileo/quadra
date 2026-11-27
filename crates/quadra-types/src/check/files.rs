//! `File` fields: the families and types they take (`File<pdf, image>`,
//! `File<"application/zip">`) and their `max 10 MB`.

use super::Checker;
use quadra_ir::files::{exact, family, family_names, show_size};
use quadra_ir::*;

/// Whether a declared type is `File`, `File?`, `[File]` or `{File}`.
fn is_file(t: &Ty) -> bool {
    match t {
        Ty::Optional(i) | Ty::List(i) | Ty::Set(i) => is_file(i),
        Ty::Path { segments, .. } => segments.len() == 1 && segments[0] == "File",
        _ => false,
    }
}

/// The largest `max`: 5 GB, the largest object S3 takes in one upload.
const LARGEST: u64 = 5 << 30;

impl<'m> Checker<'m> {
    pub(super) fn check_file_field(&mut self, e: &Entity, f: &Field) {
        let Some(spec) = &f.file else { return };
        let file = f.ty.as_ref().is_some_and(is_file);
        if let Some((bytes, span)) = spec.max {
            if !file {
                self.error(
                    span,
                    format!(
                        "`max` is the size of the largest file a `File` field takes: `{}.{}` is not a `File`",
                        e.name, f.name
                    ),
                );
            } else if bytes == 0 {
                self.error(span, "`max` takes a size: `max 10 MB` (B, KB, MB or GB)");
            } else if bytes > LARGEST {
                self.error(
                    span,
                    format!(
                        "`max {}`: at most {} (the largest file an upload takes)",
                        show_size(bytes),
                        show_size(LARGEST)
                    ),
                );
            }
        }
        for (a, span) in &spec.accept {
            match exact(a) {
                Some(t) => {
                    let ok = t.split_once('/').is_some_and(|(k, s)| {
                        !k.is_empty()
                            && !s.is_empty()
                            && t.bytes().all(|b| {
                                b.is_ascii_alphanumeric() || b"/.+-_".contains(&b)
                            })
                    });
                    if !ok {
                        self.error(
                            *span,
                            format!("`{a}` is not a type of file: write it as `\"application/zip\"`"),
                        );
                    }
                }
                None if family(a).is_none() => self.error(
                    *span,
                    format!(
                        "`{a}` is not a family of files: one of {}, or an exact type in quotes (`\"application/zip\"`)",
                        family_names()
                    ),
                ),
                None => {}
            }
        }
    }
}
