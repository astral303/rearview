use crate::error::{AppError, Result};

pub trait SemanticEmbedder {
    /// The length of every embedding this embedder returns.
    fn dimensions(&self) -> usize;
    fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>>;
    fn embed_query(&mut self, query: &str) -> Result<Option<Vec<f32>>>;
}

pub fn is_valid_embedding(embedding: &[f32], dimensions: usize) -> bool {
    embedding.len() == dimensions && embedding.iter().all(|value| value.is_finite())
}

/// Embed `query`; fail unless the embedding is `dimensions()` finite values.
pub fn embed_valid_query(
    embedder: &mut dyn SemanticEmbedder,
    query: &str,
) -> Result<Option<Vec<f32>>> {
    let dimensions = embedder.dimensions();
    match embedder.embed_query(query)? {
        Some(embedding) if !is_valid_embedding(&embedding, dimensions) => {
            Err(AppError::InvalidEmbeddings(format!(
                "the query embedding is not {dimensions} finite values"
            )))
        }
        embedding => Ok(embedding),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct QueryEmbedder {
        query: Vec<f32>,
    }

    impl SemanticEmbedder for QueryEmbedder {
        fn dimensions(&self) -> usize {
            2
        }

        fn embed_passages(&mut self, _passages: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(Vec::new())
        }

        fn embed_query(&mut self, _query: &str) -> Result<Option<Vec<f32>>> {
            Ok(Some(self.query.clone()))
        }
    }

    #[test]
    fn a_query_embedding_of_another_length_or_with_a_non_finite_value_is_an_error() {
        for query in [vec![1.0, 0.0, 0.0], vec![1.0], vec![f32::NAN, 0.0]] {
            let mut embedder = QueryEmbedder {
                query: query.clone(),
            };

            let embedded = embed_valid_query(&mut embedder, "query");

            assert!(
                matches!(embedded, Err(AppError::InvalidEmbeddings(_))),
                "{query:?}"
            );
        }
    }

    #[test]
    fn a_query_embedding_of_the_embedders_length_is_returned() {
        let mut embedder = QueryEmbedder {
            query: vec![1.0, 0.0],
        };

        let embedded = embed_valid_query(&mut embedder, "query").expect("valid query embedding");

        assert_eq!(embedded, Some(vec![1.0, 0.0]));
    }
}
