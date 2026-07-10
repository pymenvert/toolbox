//! Mini-écrivain ZIP « stored » (sans compression) pour l'export diagnostic.
//!
//! Zéro dépendance : les fichiers d'un diagnostic pèsent quelques Ko, la
//! compression n'apporterait rien face à la simplicité (et donc la
//! robustesse) d'un writer maison de ~100 lignes. Format : en-têtes locaux
//! + répertoire central + fin de répertoire, noms en UTF-8 (bit 11).

use tracing::warn;

/// Date DOS minimale (1980-01-01) : les diagnostics n'ont pas besoin
/// d'horodatage par fichier, le nom de l'archive porte la date.
const DOS_DATE: u16 = 0x0021;

/// CRC-32 (IEEE, comme le champ ZIP l'exige), bit à bit — les données sont
/// petites, pas besoin de table.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

struct Entry {
    name: String,
    crc: u32,
    size: u32,
    offset: u32,
}

/// Construit une archive en mémoire : `add()` pour chaque fichier, puis
/// `finish()` pour obtenir les octets.
pub struct ZipWriter {
    out: Vec<u8>,
    entries: Vec<Entry>,
}

impl ZipWriter {
    pub fn new() -> Self {
        Self {
            out: Vec::new(),
            entries: Vec::new(),
        }
    }

    /// Ajoute un fichier. Un contenu ou un déport hors bornes ZIP 32 bits
    /// (> 4 Gio — impossible pour un diagnostic) est ignoré avec un warn
    /// plutôt que de corrompre l'archive.
    pub fn add(&mut self, name: &str, data: &[u8]) {
        let (Ok(size), Ok(offset), Ok(name_len)) = (
            u32::try_from(data.len()),
            u32::try_from(self.out.len()),
            u16::try_from(name.len()),
        ) else {
            warn!(%name, "entrée ZIP hors bornes, ignorée");
            return;
        };
        let crc = crc32(data);
        // En-tête local.
        self.out.extend_from_slice(&0x0403_4B50_u32.to_le_bytes());
        self.out.extend_from_slice(&20_u16.to_le_bytes()); // version requise
        self.out.extend_from_slice(&0x0800_u16.to_le_bytes()); // noms UTF-8
        self.out.extend_from_slice(&0_u16.to_le_bytes()); // stored
        self.out.extend_from_slice(&0_u16.to_le_bytes()); // heure
        self.out.extend_from_slice(&DOS_DATE.to_le_bytes());
        self.out.extend_from_slice(&crc.to_le_bytes());
        self.out.extend_from_slice(&size.to_le_bytes()); // compressée
        self.out.extend_from_slice(&size.to_le_bytes()); // originale
        self.out.extend_from_slice(&name_len.to_le_bytes());
        self.out.extend_from_slice(&0_u16.to_le_bytes()); // extra
        self.out.extend_from_slice(name.as_bytes());
        self.out.extend_from_slice(data);
        self.entries.push(Entry {
            name: name.to_string(),
            crc,
            size,
            offset,
        });
    }

    /// Écrit le répertoire central + la fin d'archive et rend les octets.
    pub fn finish(mut self) -> Vec<u8> {
        let cd_offset = self.out.len();
        for entry in &self.entries {
            let name_len = u16::try_from(entry.name.len()).unwrap_or(u16::MAX);
            self.out.extend_from_slice(&0x0201_4B50_u32.to_le_bytes());
            self.out.extend_from_slice(&20_u16.to_le_bytes()); // créé par
            self.out.extend_from_slice(&20_u16.to_le_bytes()); // version requise
            self.out.extend_from_slice(&0x0800_u16.to_le_bytes());
            self.out.extend_from_slice(&0_u16.to_le_bytes()); // stored
            self.out.extend_from_slice(&0_u16.to_le_bytes()); // heure
            self.out.extend_from_slice(&DOS_DATE.to_le_bytes());
            self.out.extend_from_slice(&entry.crc.to_le_bytes());
            self.out.extend_from_slice(&entry.size.to_le_bytes());
            self.out.extend_from_slice(&entry.size.to_le_bytes());
            self.out.extend_from_slice(&name_len.to_le_bytes());
            self.out.extend_from_slice(&0_u16.to_le_bytes()); // extra
            self.out.extend_from_slice(&0_u16.to_le_bytes()); // commentaire
            self.out.extend_from_slice(&0_u16.to_le_bytes()); // disque
            self.out.extend_from_slice(&0_u16.to_le_bytes()); // attrs internes
            self.out.extend_from_slice(&0_u32.to_le_bytes()); // attrs externes
            self.out.extend_from_slice(&entry.offset.to_le_bytes());
            self.out.extend_from_slice(entry.name.as_bytes());
        }
        let cd_size = self.out.len() - cd_offset;
        let count = u16::try_from(self.entries.len()).unwrap_or(u16::MAX);
        self.out.extend_from_slice(&0x0605_4B50_u32.to_le_bytes());
        self.out.extend_from_slice(&0_u16.to_le_bytes()); // n° disque
        self.out.extend_from_slice(&0_u16.to_le_bytes()); // disque du répertoire
        self.out.extend_from_slice(&count.to_le_bytes());
        self.out.extend_from_slice(&count.to_le_bytes());
        self.out
            .extend_from_slice(&u32::try_from(cd_size).unwrap_or(u32::MAX).to_le_bytes());
        self.out
            .extend_from_slice(&u32::try_from(cd_offset).unwrap_or(u32::MAX).to_le_bytes());
        self.out.extend_from_slice(&0_u16.to_le_bytes()); // commentaire
        self.out
    }
}

impl Default for ZipWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Valeur de contrôle standard du CRC-32 IEEE.
    #[test]
    fn crc32_reference_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    /// L'archive a la structure attendue : en-têtes locaux, répertoire
    /// central, fin d'archive avec le bon compte et le bon déport.
    #[test]
    fn archive_structure_is_valid() {
        let mut zip = ZipWriter::new();
        zip.add("rapport.txt", b"bonjour");
        zip.add("etat.json", b"{}");
        let bytes = zip.finish();

        // Premier en-tête local au tout début.
        assert_eq!(&bytes[0..4], &0x0403_4B50_u32.to_le_bytes());
        // Fin d'archive : 22 derniers octets, signature + 2 entrées.
        let eocd = &bytes[bytes.len() - 22..];
        assert_eq!(&eocd[0..4], &0x0605_4B50_u32.to_le_bytes());
        assert_eq!(u16::from_le_bytes([eocd[8], eocd[9]]), 2);
        assert_eq!(u16::from_le_bytes([eocd[10], eocd[11]]), 2);
        // Le déport du répertoire central pointe sur sa signature.
        let cd_offset = u32::from_le_bytes([eocd[16], eocd[17], eocd[18], eocd[19]]) as usize;
        assert_eq!(
            &bytes[cd_offset..cd_offset + 4],
            &0x0201_4B50_u32.to_le_bytes()
        );
        // Les noms et les contenus sont bien dans l'archive.
        let haystack = String::from_utf8_lossy(&bytes);
        assert!(haystack.contains("rapport.txt"));
        assert!(haystack.contains("bonjour"));
        assert!(haystack.contains("etat.json"));
    }

    /// Une archive vide reste une archive ZIP valide (fin d'archive seule).
    #[test]
    fn empty_archive_is_just_an_eocd() {
        let bytes = ZipWriter::new().finish();
        assert_eq!(bytes.len(), 22);
        assert_eq!(&bytes[0..4], &0x0605_4B50_u32.to_le_bytes());
    }
}
