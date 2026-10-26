//! Portable formatting recipes. Definitions occur once; operation leaves carry
//! only a recipe ID and exact typed values, including pre-rendered opaque slots.
use super::*;
use crate::subscriber::format::{ArgumentKind, Format};
use introspect_fmt::{OwnedTemplate, TemplateArgument, TemplatePiece};

pub(super) fn encode(recipe: &Format) -> Mp {
    array([
        recipe.template.source().into(),
        array(recipe.template.arguments().iter().map(|a| {
            array([
                text(a.name),
                a.kind.into(),
                optional(a.slot.map(|s| s as u64)),
            ])
        })),
        array(recipe.template.pieces().iter().map(|p| match p {
            TemplatePiece::Literal(s) => array(["literal".into(), s.as_str().into()]),
            TemplatePiece::Argument {
                index,
                spec,
                next,
                rendered,
            } => array([
                "argument".into(),
                (*index as u64).into(),
                spec.as_str().into(),
                (*next as u64).into(),
                optional(rendered.map(|s| s as u64)),
            ]),
        })),
        (recipe.template.value_count() as u64).into(),
        array(recipe.kinds.iter().map(|kind| kind.tag().into())),
    ])
}

fn index(value: &Mp) -> io::Result<usize> {
    usize::try_from(number(value)?).map_err(io::Error::other)
}
fn opt_index(value: &Mp) -> io::Result<Option<usize>> {
    if value.is_nil() {
        Ok(None)
    } else {
        index(value).map(Some)
    }
}

pub(super) fn decode(value: &Mp) -> io::Result<Arc<Format>> {
    let v = items(value)?;
    if v.len() != 5 {
        return Err(invalid("invalid format recipe"));
    }
    let arguments = items(&v[1])?
        .iter()
        .map(|value| {
            let a = items(value)?;
            if a.len() != 3 {
                return Err(invalid("invalid format argument schema"));
            }
            Ok(TemplateArgument {
                name: if a[0].is_nil() {
                    None
                } else {
                    Some(leak(string(&a[0])?))
                },
                kind: leak(string(&a[1])?),
                slot: opt_index(&a[2])?,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let pieces = items(&v[2])?
        .iter()
        .map(|value| {
            let p = items(value)?;
            match p.first().map(string).transpose()? {
                Some("literal") if p.len() == 2 => {
                    Ok(TemplatePiece::Literal(string(&p[1])?.into()))
                }
                Some("argument") if p.len() == 5 => Ok(TemplatePiece::Argument {
                    index: index(&p[1])?,
                    spec: string(&p[2])?.into(),
                    next: index(&p[3])?,
                    rendered: opt_index(&p[4])?,
                }),
                _ => Err(invalid("invalid format recipe piece")),
            }
        })
        .collect::<io::Result<Vec<_>>>()?;
    let count = index(&v[3])?;
    let kinds = items(&v[4])?
        .iter()
        .map(|kind| {
            ArgumentKind::from_tag(string(kind)?)
                .ok_or_else(|| invalid("unknown format argument kind"))
        })
        .collect::<io::Result<Vec<_>>>()?;
    if kinds.len() != count {
        return Err(invalid("format recipe value count differs"));
    }
    let template = OwnedTemplate::from_parts(leak(string(&v[0])?), arguments, pieces, count)
        .map_err(|e| invalid(e.to_string()))?;
    Ok(Arc::new(Format { template, kinds }))
}

pub(super) struct Definition {
    pub site: usize,
    pub field: usize,
    pub recipe: Arc<Format>,
}

pub(super) fn decode_values(
    value: &Mp,
    site: usize,
    definitions: &[Definition],
    fields: &[Option<InputValue>],
) -> io::Result<Vec<CapturedFormat>> {
    let mut result = Vec::<CapturedFormat>::new();
    for value in items(value)? {
        let v = items(value)?;
        if v.len() != 2 {
            return Err(invalid("invalid captured format"));
        }
        let definition = definitions
            .get(index(&v[0])?)
            .ok_or_else(|| invalid("unknown format recipe"))?;
        if definition.site != site || result.iter().any(|f| f.field == definition.field) {
            return Err(invalid("format recipe callsite/field differs"));
        }
        let values = decode_fields(&v[1])?
            .into_iter()
            .map(|v| v.ok_or_else(|| invalid("missing format argument")))
            .collect::<io::Result<Vec<_>>>()?;
        if !definition.recipe.accepts(&values) {
            return Err(invalid("format argument type differs"));
        }
        let rendered = definition
            .recipe
            .render(&values.iter().cloned().map(Some).collect::<Vec<_>>())
            .map_err(|e| invalid(e.to_string()))?;
        if fields.get(definition.field) != Some(&Some(InputValue::Text(rendered))) {
            return Err(invalid("format arguments disagree with rendered field"));
        }
        result.push(CapturedFormat {
            field: definition.field,
            recipe: definition.recipe.clone(),
            values,
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_recipes_reject_bad_slots_types_and_rendered_values() {
        // Validate imported data before replay reaches any indexing/schema asserts.
        let (recipe, values) = introspect_fmt::inspect_format!(
            |view| Format::capture_new(&view).unwrap(),
            "value {:#06x}",
            7u16
        );
        let encoded = encode(&recipe);
        let imported = decode(&encoded).unwrap();
        let definitions = vec![Definition {
            site: 2,
            field: 0,
            recipe: imported,
        }];
        let arguments = array([array([
            0.into(),
            encode_fields(&values.into_iter().map(Some).collect::<Vec<_>>()),
        ])]);
        assert!(
            decode_values(
                &arguments,
                2,
                &definitions,
                &[Some(InputValue::Text("value 0x0007".into()))]
            )
            .is_ok()
        );
        assert!(
            decode_values(
                &arguments,
                3,
                &definitions,
                &[Some(InputValue::Text("value 0x0007".into()))]
            )
            .is_err()
        );
        assert!(
            decode_values(
                &arguments,
                2,
                &definitions,
                &[Some(InputValue::Text("wrong".into()))]
            )
            .is_err()
        );
        let wrong_type = array([array([
            0.into(),
            encode_fields(&[Some(InputValue::U64(7))]),
        ])]);
        assert!(
            decode_values(
                &wrong_type,
                2,
                &definitions,
                &[Some(InputValue::Text("value 0x0007".into()))]
            )
            .is_err()
        );

        let mut bad_slot = encoded;
        let Mp::Array(parts) = &mut bad_slot else {
            unreachable!()
        };
        let Mp::Array(args) = &mut parts[1] else {
            unreachable!()
        };
        let Mp::Array(argument) = &mut args[0] else {
            unreachable!()
        };
        argument[2] = u64::MAX.into();
        assert!(decode(&bad_slot).is_err());
    }
}
