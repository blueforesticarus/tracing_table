//! Portable formatting recipes. Definitions occur once; operation leaves carry
//! only a recipe ID and exact typed values, including pre-rendered opaque slots.
use super::*;
use crate::subscriber::value::{accepts_inputs, render_inputs};
use introspect_fmt::{ArgumentKind, Recipe};

/// v4: the template source, every argument's name, and every argument's kind.
/// Loading reparses the source, so the dump never depends on parsed layouts.
pub(super) fn encode(recipe: &Recipe, names: &[Option<&str>]) -> Mp {
    array([
        recipe.template().source().into(),
        array(names.iter().map(|name| text(*name))),
        array(recipe.kinds().iter().map(|kind| kind.name().into())),
    ])
}

fn index(value: &Mp) -> io::Result<usize> {
    usize::try_from(number(value)?).map_err(io::Error::other)
}

fn name(value: &Mp) -> io::Result<Option<&str>> {
    if value.is_nil() {
        Ok(None)
    } else {
        string(value).map(Some)
    }
}

pub(super) fn decode(value: &Mp) -> io::Result<Recipe> {
    let v = items(value)?;
    if v.len() != 3 {
        return Err(invalid("invalid format recipe"));
    }
    let names = items(&v[1])?
        .iter()
        .map(name)
        .collect::<io::Result<Vec<_>>>()?;
    let kinds = items(&v[2])?
        .iter()
        .map(|kind| {
            ArgumentKind::from_name(string(kind)?)
                .ok_or_else(|| invalid("unknown format argument kind"))
        })
        .collect::<io::Result<Vec<_>>>()?;
    if names.len() != kinds.len() {
        return Err(invalid("format argument names and kinds differ"));
    }
    Recipe::parse(string(&v[0])?, &names, &kinds).map_err(|e| invalid(e.to_string()))
}

pub(super) struct Definition {
    pub site: usize,
    pub field: usize,
    pub recipe: Recipe,
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
        if !accepts_inputs(&definition.recipe, &values) {
            return Err(invalid("format argument type differs"));
        }
        let rendered =
            render_inputs(&definition.recipe, &values).map_err(|e| invalid(e.to_string()))?;
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
    use crate::subscriber::value::capture_inputs;

    #[test]
    fn imported_recipes_reject_bad_kinds_names_types_and_rendered_values() {
        // Validate imported data before replay reaches any indexing/schema asserts.
        let (recipe, names, values) = introspect_fmt::inspect_format!(
            |view| {
                let recipe = Recipe::new(&view).unwrap();
                let values = capture_inputs(&recipe, &view).unwrap();
                let names: Vec<_> = view.args().iter().map(|arg| arg.name()).collect();
                (recipe, names, values)
            },
            "value {:#06x}",
            7u16
        );
        let encoded = encode(&recipe, &names);
        let imported = decode(&encoded).unwrap();
        assert_eq!(imported, recipe);
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

        let mut bad_kind = encoded.clone();
        let Mp::Array(parts) = &mut bad_kind else {
            unreachable!()
        };
        parts[2] = array(["unknown".into()]);
        assert!(decode(&bad_kind).is_err());
        let mut missing_name = encoded.clone();
        let Mp::Array(parts) = &mut missing_name else {
            unreachable!()
        };
        parts[1] = array([]);
        assert!(decode(&missing_name).is_err());
        // A placeholder that references a missing argument cannot be imported.
        assert!(
            decode(&array([
                "{} {}".into(),
                array([Mp::Nil]),
                array(["u8".into()])
            ]))
            .is_err()
        );
    }
}
