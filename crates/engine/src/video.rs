//! Frame vidéo décodée, partagée entre le backend (producteur) et la
//! fenêtre de sortie (consommateur) via un canal `watch` — la fenêtre ne
//! peint que la dernière frame reçue, les frames intermédiaires en retard
//! sont naturellement écrasées.

use std::sync::Arc;

/// Une frame RGBA 8 bits (4 octets par pixel, lignes de haut en bas).
///
/// Le tampon est partagé (`Arc`) : cloner une frame est gratuit, le backend
/// n'attend jamais le rendu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

impl VideoFrame {
    /// Construit une frame en vérifiant la cohérence tampon/dimensions.
    /// `None` si les tailles ne correspondent pas (frame corrompue).
    pub fn new(width: u32, height: u32, rgba: Arc<[u8]>) -> Option<Self> {
        let expected = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        (width > 0 && height > 0 && rgba.len() == expected).then_some(Self {
            width,
            height,
            rgba,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_checks_buffer_size() {
        let ok: Arc<[u8]> = vec![0u8; 2 * 3 * 4].into();
        assert!(VideoFrame::new(2, 3, ok.clone()).is_some());
        assert!(
            VideoFrame::new(3, 3, ok.clone()).is_none(),
            "tampon trop court"
        );
        assert!(VideoFrame::new(0, 3, ok).is_none(), "dimension nulle");
    }
}
