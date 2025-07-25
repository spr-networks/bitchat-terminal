// Integration layer between the existing encryption service and new Noise protocol
// This provides backward compatibility while adding Noise support

use crate::noise::{NoiseSessionManager, NoiseError};
use std::sync::{Arc, RwLock};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use sha2::{Sha256, Digest};
use pbkdf2::pbkdf2_hmac;
use rand::rngs::OsRng;
use ed25519_dalek::{SigningKey, Signature, Signer, VerifyingKey, Verifier};
// Use snow's built-in ChaCha20-Poly1305 cipher implementation
use snow::resolvers::{DefaultResolver, CryptoResolver};
use snow::params::CipherChoice;

pub struct NoiseIntegrationService {
    session_manager: NoiseSessionManager,

    // Legacy compatibility
    peer_public_keys: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    peer_fingerprints: Arc<RwLock<HashMap<String, String>>>,

    // Rate limiting (simple implementation)
    last_handshake: Arc<RwLock<HashMap<String, u64>>>,
    last_message: Arc<RwLock<HashMap<String, u64>>>,

    // Ed25519 signing key for message authentication
    signing_key: Option<SigningKey>,
}

impl NoiseIntegrationService {
    pub fn new() -> Result<Self, NoiseError> {
        let session_manager = NoiseSessionManager::new()?;

        Ok(NoiseIntegrationService {
            session_manager,
            peer_public_keys: Arc::new(RwLock::new(HashMap::new())),
            peer_fingerprints: Arc::new(RwLock::new(HashMap::new())),
            last_handshake: Arc::new(RwLock::new(HashMap::new())),
            last_message: Arc::new(RwLock::new(HashMap::new())),
            signing_key: None,
        })
    }

    pub fn with_signing_key(identity_key_bytes: &[u8]) -> Result<Self, NoiseError> {
        let session_manager = NoiseSessionManager::new()?;

        let signing_key = if identity_key_bytes.len() == 32 {
            let key_array: [u8; 32] = identity_key_bytes.try_into().unwrap();
            Some(SigningKey::from_bytes(&key_array))
        } else {
            None
        };

        Ok(NoiseIntegrationService {
            session_manager,
            peer_public_keys: Arc::new(RwLock::new(HashMap::new())),
            peer_fingerprints: Arc::new(RwLock::new(HashMap::new())),
            last_handshake: Arc::new(RwLock::new(HashMap::new())),
            last_message: Arc::new(RwLock::new(HashMap::new())),
            signing_key,
        })
    }

    /// Create a new NoiseIntegrationService from an existing static key
    /// This is useful for restoring a service from persisted key material

    pub fn from_existing_key(static_key: Vec<u8>) -> Result<Self, NoiseError> {
        let session_manager = NoiseSessionManager::from_static_key(static_key)?;

        Ok(NoiseIntegrationService {
            session_manager,
            peer_public_keys: Arc::new(RwLock::new(HashMap::new())),
            peer_fingerprints: Arc::new(RwLock::new(HashMap::new())),
            last_handshake: Arc::new(RwLock::new(HashMap::new())),
            last_message: Arc::new(RwLock::new(HashMap::new())),
            signing_key: None,
        })
    }

    // Get our static public key for sharing
    pub fn get_static_public_key(&self) -> Vec<u8> {
        self.session_manager.get_static_public_key().to_vec()
    }

    // Get our identity fingerprint
    pub fn get_identity_fingerprint(&self) -> String {
        self.session_manager.get_identity_fingerprint()
    }

    // Initiate handshake with a peer
    pub fn initiate_handshake(&self, peer_id: &str) -> Result<Vec<u8>, NoiseError> {
        // Simple rate limiting - max 1 handshake per 10 seconds per peer
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        {
            let mut last_handshake = self.last_handshake.write().unwrap();
            if let Some(&last_time) = last_handshake.get(peer_id) {
                let min_interval = if cfg!(test) { 0 } else { 10000 }; // No rate limiting for tests, 10s for production
                if now - last_time < min_interval {
                    return Err(NoiseError::RateLimitExceeded);
                }
            }
            last_handshake.insert(peer_id.to_string(), now);
        }

        let handshake_data = self.session_manager.initiate_handshake(peer_id)?;

        // Return raw handshake data without JSON wrapper (matching Swift)
        Ok(handshake_data)
    }

    // Process incoming handshake message
    pub fn process_handshake_message(&self, peer_id: &str, data: &[u8]) -> Result<Option<Vec<u8>>, NoiseError> {
        // Use raw handshake data directly (matching Swift)
        let payload = data.to_vec();

        // Debug the handshake message being processed
        println!("[NOISE_INTEGRATION_DEBUG] Processing handshake from {} ({} bytes)", peer_id, payload.len());
        crate::noise::debug_handshake_message(&payload, &format!("NoiseIntegration for {}", peer_id));

        // No rate limiting for processing handshake responses
        // Rate limiting is only applied when initiating handshakes
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;

        let response = self.session_manager.handle_incoming_handshake(peer_id, &payload)?;

        if let Some(response_data) = response {
            // Update peer info if session is now established
            if self.session_manager.has_established_session(peer_id) {
                self.update_peer_info(peer_id);
            }

            // Return raw response data without JSON wrapper (matching Swift)
            Ok(Some(response_data))
        } else {
            // Handshake complete
            if self.session_manager.has_established_session(peer_id) {
                self.update_peer_info(peer_id);
            }
            Ok(None)
        }
    }

    // Check if we have an established session
    pub fn has_established_session(&self, peer_id: &str) -> bool {
        self.session_manager.has_established_session(peer_id)
    }

    // Encrypt data for a peer
    pub fn encrypt_for_peer(&self, peer_id: &str, data: &[u8]) -> Result<Vec<u8>, NoiseError> {
        // Simple rate limiting - max 100 messages per second per peer
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        {
            let mut last_message = self.last_message.write().unwrap();
            if let Some(&last_time) = last_message.get(peer_id) {
                let min_interval = if cfg!(test) { 0 } else { 10 }; // No rate limiting for tests, 10ms for production
                if now - last_time < min_interval {
                    return Err(NoiseError::RateLimitExceeded);
                }
            }
            last_message.insert(peer_id.to_string(), now);
        }

        let encrypted = self.session_manager.encrypt(peer_id, data)?;

        // Return raw encrypted data without JSON wrapper (matching Swift)
        Ok(encrypted)
    }

    // Decrypt data from a peer
    pub fn decrypt_from_peer(&self, peer_id: &str, data: &[u8]) -> Result<Vec<u8>, NoiseError> {
        // Use raw encrypted data directly (matching Swift)
        self.session_manager.decrypt(peer_id, data)
    }

    // Get peer's public key
    /// Get the public key for a peer (from either Noise session or legacy storage)

    pub fn get_peer_public_key(&self, peer_id: &str) -> Option<Vec<u8>> {
        // Try Noise session first
        if let Some(key) = self.session_manager.get_remote_static_key(peer_id) {
            return Some(key);
        }

        // Fall back to stored keys
        let keys = self.peer_public_keys.read().unwrap();
        keys.get(peer_id).cloned()
    }

    // Get peer's fingerprint
    pub fn get_peer_fingerprint(&self, peer_id: &str) -> Option<String> {
        // Try Noise session first
        if let Some(fingerprint) = self.session_manager.get_peer_fingerprint(peer_id) {
            return Some(fingerprint);
        }

        // Fall back to stored fingerprints
        let fingerprints = self.peer_fingerprints.read().unwrap();
        fingerprints.get(peer_id).cloned()
    }

    // Remove a peer
    /// Remove a peer from all session and key storage

    pub fn remove_peer(&self, peer_id: &str) {
        self.session_manager.remove_session(peer_id);

        let mut keys = self.peer_public_keys.write().unwrap();
        keys.remove(peer_id);

        let mut fingerprints = self.peer_fingerprints.write().unwrap();
        fingerprints.remove(peer_id);
    }

    // Get all established sessions
    /// Get list of all peers with established Noise sessions

    pub fn get_established_sessions(&self) -> Vec<String> {
        self.session_manager.get_established_sessions()
    }

    // Cleanup expired sessions
    /// Clean up any expired Noise sessions

    pub fn cleanup_expired_sessions(&self) {
        self.session_manager.cleanup_expired_sessions();
    }

    // Update peer information after successful handshake
    fn update_peer_info(&self, peer_id: &str) {
        if let Some(public_key) = self.session_manager.get_remote_static_key(peer_id) {
            let fingerprint = {
                let hash = Sha256::digest(&public_key);
                hex::encode(hash)
            };

            {
                let mut keys = self.peer_public_keys.write().unwrap();
                keys.insert(peer_id.to_string(), public_key);
            }

            {
                let mut fingerprints = self.peer_fingerprints.write().unwrap();
                fingerprints.insert(peer_id.to_string(), fingerprint);
            }
        }
    }

    // Legacy compatibility methods

    // Store a peer's public key (for legacy compatibility)
    /// Store a peer's public key for legacy compatibility

    pub fn store_peer_public_key(&self, peer_id: &str, public_key: Vec<u8>) {
        let fingerprint = {
            let hash = Sha256::digest(&public_key);
            hex::encode(hash)
        };

        {
            let mut keys = self.peer_public_keys.write().unwrap();
            keys.insert(peer_id.to_string(), public_key);
        }

        {
            let mut fingerprints = self.peer_fingerprints.write().unwrap();
            fingerprints.insert(peer_id.to_string(), fingerprint);
        }
    }

    // Check if noise is supported for a peer (always true for new implementation)
    /// Check if Noise protocol is supported for a peer (always true in this implementation)

    pub fn supports_noise(&self, _peer_id: &str) -> bool {
        true
    }

    // Migration helper: check if we should use noise for this peer
    /// Migration helper: determine if Noise should be used for this peer

    pub fn should_use_noise(&self, peer_id: &str) -> bool {
        // Use noise if we have an established session or if it's a new peer
        self.has_established_session(peer_id) || !self.has_legacy_keys(peer_id)
    }

    // Check if we have legacy keys for a peer
    /// Check if we have legacy keys stored for a peer

    fn has_legacy_keys(&self, peer_id: &str) -> bool {
        let keys = self.peer_public_keys.read().unwrap();
        keys.contains_key(peer_id)
    }

    // === Channel Encryption (Swift-compatible) ===

    /// Derive channel encryption key using PBKDF2 (matching Swift implementation)
    /// Uses 210,000 iterations and salt format: "bitchat-channel-{channel_name}"
    pub fn derive_channel_key(password: &str, channel_name: &str) -> [u8; 32] {
        let salt = format!("bitchat-channel-{}", channel_name);
        let mut key = [0u8; 32];
        pbkdf2_hmac::<Sha256>(
            password.as_bytes(),
            salt.as_bytes(),
            210_000,  // Swift uses 210,000 iterations (OWASP recommended)
            &mut key,
        );
        key
    }

    /// Encrypt data with ChaCha20-Poly1305 using channel key (Swift-compatible)
    /// Format: nonce (12 bytes) + ciphertext + tag (16 bytes) - exactly matching Swift
    /// Uses snow's built-in CipherChaChaPoly implementation
    pub fn encrypt_with_channel_key(&self, data: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, NoiseError> {
        // Use snow's resolver to get ChaCha20-Poly1305 cipher
        let resolver = DefaultResolver;
        let mut cipher = resolver.resolve_cipher(&CipherChoice::ChaChaPoly)
            .ok_or_else(|| NoiseError::EncryptionError("Failed to create cipher".to_string()))?;

        // Generate random nonce (12 bytes for ChaCha20-Poly1305)
        let mut nonce = [0u8; 12];
        use rand::RngCore;
        OsRng.fill_bytes(&mut nonce);

        // Set key
        cipher.set(key);

        // Prepare buffer for snow's API - need extra space for the authentication tag
        let mut ciphertext = vec![0u8; data.len() + 16]; // 16 bytes for ChaCha20-Poly1305 tag

        // Encrypt data using snow's API: encrypt(nonce: u64, authtext: &[u8], plaintext: &[u8], out: &mut [u8]) -> usize
        let nonce_u64 = u64::from_le_bytes(nonce[..8].try_into().unwrap());
        let ciphertext_len = cipher.encrypt(nonce_u64, &[], data, &mut ciphertext);

        // Truncate to actual ciphertext length (includes tag)
        ciphertext.truncate(ciphertext_len);

        // Swift format: nonce (12 bytes) + ciphertext + tag (16 bytes)
        let mut result = Vec::with_capacity(12 + ciphertext.len());
        result.extend_from_slice(&nonce);
        result.extend_from_slice(&ciphertext);

        Ok(result)
    }

    /// Decrypt data with ChaCha20-Poly1305 using channel key (Swift-compatible)
    /// Uses snow's built-in CipherChaChaPoly implementation
    pub fn decrypt_with_channel_key(&self, data: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, NoiseError> {
        if data.len() < 28 { // 12 (nonce) + 16 (tag) minimum
            return Err(NoiseError::EncryptionError("Invalid encrypted data length".to_string()));
        }

        // Extract components: nonce (12) + ciphertext + tag (16)
        let nonce = &data[0..12];
        let ciphertext_with_tag = &data[12..]; // ciphertext + tag

        // Use snow's resolver to get ChaCha20-Poly1305 cipher
        let resolver = DefaultResolver;
        let mut cipher = resolver.resolve_cipher(&CipherChoice::ChaChaPoly)
            .ok_or_else(|| NoiseError::EncryptionError("Failed to create cipher".to_string()))?;

        // Set key
        cipher.set(key);

        // Prepare buffer for snow's API
        let mut plaintext = vec![0u8; ciphertext_with_tag.len()];

        // Decrypt data using snow's API: decrypt(nonce: u64, authtext: &[u8], ciphertext: &[u8], out: &mut [u8]) -> Result<usize, ()>
        let nonce_u64 = u64::from_le_bytes(nonce[..8].try_into().unwrap());
        let result_len = cipher.decrypt(nonce_u64, &[], ciphertext_with_tag, &mut plaintext)
            .map_err(|_| NoiseError::EncryptionError("Channel decryption failed".to_string()))?;

        // Resize to actual plaintext length
        plaintext.truncate(result_len);

        Ok(plaintext)
    }

    /// Sign data using Ed25519 for message authentication
    /// Returns None if no signing key is available (matching Swift signData pattern)
    pub fn sign(&self, data: &[u8]) -> Option<Vec<u8>> {
        if let Some(ref signing_key) = self.signing_key {
            // Use proper Ed25519 signing
            let signature: Signature = signing_key.sign(data);
            Some(signature.to_bytes().to_vec())
        } else {
            // No signing key available - return None
            // This should not happen in normal operation since identity keys are auto-generated
            None
        }
    }

    /// Verify a signature using the peer's public key
    /// Returns true if the signature is valid, false otherwise
    pub fn verify_signature(&self, signature: &[u8], data: &[u8], public_key: &[u8]) -> bool {
        // Validate signature length (Ed25519 signatures are 64 bytes)
        if signature.len() != 64 {
            return false;
        }

        // Validate public key length (Ed25519 public keys are 32 bytes)
        if public_key.len() != 32 {
            return false;
        }

        // Convert signature bytes to Ed25519 signature
        let signature_array: [u8; 64] = match signature.try_into() {
            Ok(arr) => arr,
            Err(_) => return false,
        };
        let ed25519_signature = Signature::from_bytes(&signature_array);

        // Convert public key bytes to Ed25519 verifying key
        let public_key_array: [u8; 32] = match public_key.try_into() {
            Ok(arr) => arr,
            Err(_) => return false,
        };
        let verifying_key = match VerifyingKey::from_bytes(&public_key_array) {
            Ok(key) => key,
            Err(_) => return false,
        };

        // Verify the signature
        verifying_key.verify(data, &ed25519_signature).is_ok()
    }

    /// Get peer's public key data (alias for get_peer_public_key)
    pub fn get_peer_public_key_data(&self, peer_id: &str) -> Option<Vec<u8>> {
        self.get_peer_public_key(peer_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_integration_service_creation() {
        let service = NoiseIntegrationService::new();
        assert!(service.is_ok());
    }

    #[test]
    fn test_handshake_integration() {
        let initiator = NoiseIntegrationService::new().unwrap();
        let responder = NoiseIntegrationService::new().unwrap();

        // Initiator starts handshake
        let msg1 = initiator.initiate_handshake("responder").unwrap();

        // Responder processes and responds
        let response = responder.process_handshake_message("initiator", &msg1).unwrap();
        assert!(response.is_some());

        // Continue handshake
        let msg2 = response.unwrap();
        let response2 = initiator.process_handshake_message("responder", &msg2).unwrap();
        assert!(response2.is_some());

        // Final message
        let msg3 = response2.unwrap();
        let response3 = responder.process_handshake_message("initiator", &msg3).unwrap();
        assert!(response3.is_none()); // Handshake complete

        // Both should have established sessions
        assert!(initiator.has_established_session("responder"));
        assert!(responder.has_established_session("initiator"));
    }

    #[test]
    fn test_encryption_integration() {
        let initiator = NoiseIntegrationService::new().unwrap();
        let responder = NoiseIntegrationService::new().unwrap();

        // Complete handshake
        let msg1 = initiator.initiate_handshake("responder").unwrap();
        let msg2 = responder.process_handshake_message("initiator", &msg1).unwrap().unwrap();
        let msg3 = initiator.process_handshake_message("responder", &msg2).unwrap().unwrap();
        let _ = responder.process_handshake_message("initiator", &msg3).unwrap();

        // Test encryption/decryption
        let plaintext = b"Hello, Noise Integration!";
        let encrypted = initiator.encrypt_for_peer("responder", plaintext).unwrap();
        let decrypted = responder.decrypt_from_peer("initiator", &encrypted).unwrap();

        assert_eq!(plaintext, decrypted.as_slice());
    }

    #[test]
    fn test_automatic_handshake_initiation() {
        let initiator = NoiseIntegrationService::new().unwrap();

        // Trying to encrypt for a peer without established session should fail
        let plaintext = b"Hello, World!";
        let result = initiator.encrypt_for_peer("unknown_peer", plaintext);
        assert!(matches!(result, Err(NoiseError::SessionNotFound)));

        // But we should be able to initiate a handshake
        let handshake_msg = initiator.initiate_handshake("unknown_peer").unwrap();
        assert!(!handshake_msg.is_empty());

        // After initiating, there should be a session in progress (but not established)
        assert!(!initiator.has_established_session("unknown_peer"));
    }

    #[test]
    fn test_signing_with_and_without_key() {
        // Test without signing key (should return None)
        let service_no_key = NoiseIntegrationService::new().unwrap();
        let data = b"test message";
        let signature_no_key = service_no_key.sign(data);
        assert!(signature_no_key.is_none()); // Should be None when no signing key

        // Test with signing key
        let identity_key = [1u8; 32]; // Test key
        let service_with_key = NoiseIntegrationService::with_signing_key(&identity_key).unwrap();
        let signature_with_key = service_with_key.sign(data);
        assert!(signature_with_key.is_some()); // Should have a signature
        assert_eq!(signature_with_key.as_ref().unwrap().len(), 64); // Ed25519 signature is 64 bytes

        // Signatures should be different
        assert_ne!(signature_no_key, signature_with_key);
    }

    #[test]
    fn test_signature_verification_valid() {
        // Create service with signing key
        let identity_key = [42u8; 32]; // Test key
        let service = NoiseIntegrationService::with_signing_key(&identity_key).unwrap();
        
        // Sign some data
        let data = b"Hello, signature verification!";
        let signature = service.sign(data).expect("Should have signature");
        
        // Extract public key from signing key for verification
        let signing_key = SigningKey::from_bytes(&identity_key);
        let verifying_key = signing_key.verifying_key();
        let public_key_bytes = verifying_key.to_bytes();
        
        // Verify the signature
        let is_valid = service.verify_signature(&signature, data, &public_key_bytes);
        assert!(is_valid, "Valid signature should verify successfully");
    }

    #[test]
    fn test_signature_verification_invalid_signature() {
        let service = NoiseIntegrationService::new().unwrap();
        let data = b"test data";
        let public_key = [1u8; 32];
        let invalid_signature = [0u8; 64]; // Invalid signature
        
        let is_valid = service.verify_signature(&invalid_signature, data, &public_key);
        assert!(!is_valid, "Invalid signature should not verify");
    }

    #[test]
    fn test_signature_verification_wrong_data() {
        // Create service with signing key
        let identity_key = [123u8; 32];
        let service = NoiseIntegrationService::with_signing_key(&identity_key).unwrap();
        
        // Sign some data
        let original_data = b"original message";
        let signature = service.sign(original_data).expect("Should have signature");
        
        // Try to verify with different data
        let different_data = b"different message";
        
        // Extract public key
        let signing_key = SigningKey::from_bytes(&identity_key);
        let public_key_bytes = signing_key.verifying_key().to_bytes();
        
        let is_valid = service.verify_signature(&signature, different_data, &public_key_bytes);
        assert!(!is_valid, "Signature should not verify with different data");
    }

    #[test]
    fn test_signature_verification_invalid_signature_length() {
        let service = NoiseIntegrationService::new().unwrap();
        let data = b"test data";
        let public_key = [1u8; 32];
        
        // Test too short signature
        let short_signature = [0u8; 32];
        let is_valid = service.verify_signature(&short_signature, data, &public_key);
        assert!(!is_valid, "Short signature should not verify");
        
        // Test too long signature
        let long_signature = [0u8; 96];
        let is_valid = service.verify_signature(&long_signature, data, &public_key);
        assert!(!is_valid, "Long signature should not verify");
    }

    #[test]
    fn test_signature_verification_invalid_public_key_length() {
        let service = NoiseIntegrationService::new().unwrap();
        let data = b"test data";
        let signature = [0u8; 64];
        
        // Test too short public key
        let short_public_key = [0u8; 16];
        let is_valid = service.verify_signature(&signature, data, &short_public_key);
        assert!(!is_valid, "Short public key should not verify");
        
        // Test too long public key
        let long_public_key = [0u8; 64];
        let is_valid = service.verify_signature(&signature, data, &long_public_key);
        assert!(!is_valid, "Long public key should not verify");
    }

    #[test]
    fn test_signature_verification_malformed_public_key() {
        let service = NoiseIntegrationService::new().unwrap();
        let data = b"test data";
        let signature = [0u8; 64];
        let malformed_public_key = [255u8; 32]; // Invalid Ed25519 public key
        
        let is_valid = service.verify_signature(&signature, data, &malformed_public_key);
        assert!(!is_valid, "Malformed public key should not verify");
    }
}
