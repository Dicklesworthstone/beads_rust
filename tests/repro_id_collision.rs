use beads_rust::util::id::{IdGenerator, IdConfig};
use chrono::Utc;

#[test]
fn test_id_generator_fallback_collision() {
    let config = IdConfig {
        prefix: "bd".to_string(),
        min_hash_length: 3,
        max_hash_length: 3, // Force max length quickly
        max_collision_prob: 0.0,
    };
    let generator = IdGenerator::new(config);
    
    // This set represents existing IDs.
    // We will populate it such that all normal candidates collide.
    let mut existing_ids = std::collections::HashSet::new();
    
    let title = "Test Issue";
    let now = Utc::now();
    
    // pre-calculate what the generator would produce and block them
    // The generator tries nonces 0..10 at length 3.
    for nonce in 0..10 {
        let candidate = generator.generate_candidate(title, None, None, now, nonce, 3);
        existing_ids.insert(candidate);
    }
    
    // Also block the fallback ID!
    // The fallback uses nonce 0 and length 12.
    let fallback_candidate = generator.generate_candidate(title, None, None, now, 0, 12);
    existing_ids.insert(fallback_candidate.clone());
    
    // Now call generate. It should loop through nonces, fail, hit fallback.
    // The fallback should produce `fallback_candidate`.
    // IF the bug exists, it will return `fallback_candidate` WITHOUT checking `exists`.
    // Since `fallback_candidate` is in `existing_ids`, this means it returned a duplicate.
    
    let generated = generator.generate(title, None, None, now, 0, |id| existing_ids.contains(id));
    
    // If the generator was safe, it would have found ANOTHER ID (e.g. by trying more nonces or random).
    // If it returns the blocked fallback ID, it failed the safety contract.
    assert!(!existing_ids.contains(&generated), "Generator returned an existing ID: {}", generated);
}
