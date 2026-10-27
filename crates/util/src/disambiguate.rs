use std::collections::HashMap;
use std::hash::Hash;

/// Computes the minimum detail level needed for each item so that no two items
/// share the same description. Items whose descriptions are unique at level 0
/// stay at 0; items that collide get their detail level incremented until either
/// the collision is resolved or increasing the level no longer changes the
/// description (preventing infinite loops for truly identical items).
///
/// The `get_description` closure must return a sequence that eventually reaches
/// a "fixed point" where increasing `detail` no longer changes the output. If
/// an item reaches its fixed point, it is assumed it will no longer change and
/// will no longer be checked for collisions.
pub fn compute_disambiguation_details<T, D>(
    items: &[T],
    get_description: impl Fn(&T, usize) -> D,
) -> Vec<usize>
where
    D: Eq + Hash + Clone,
{
    let mut details = vec![0usize; items.len()];
    let mut descriptions: HashMap<D, Vec<usize>> = HashMap::default();
    let mut current_descriptions: Vec<D> =
        items.iter().map(|item| get_description(item, 0)).collect();

    loop {
        let mut any_collisions = false;

        for (index, (item, &detail)) in items.iter().zip(&details).enumerate() {
            if detail > 0 {
                let new_description = get_description(item, detail);
                if new_description == current_descriptions[index] {
                    continue;
                }
                current_descriptions[index] = new_description;
            }
            descriptions
                .entry(current_descriptions[index].clone())
                .or_insert_with(Vec::new)
                .push(index);
        }

        for (_, indices) in descriptions.drain() {
            if indices.len() > 1 {
                any_collisions = true;
                for index in indices {
                    details[index] += 1;
                }
            }
        }

        if !any_collisions {
            break;
        }
    }

    details
}

