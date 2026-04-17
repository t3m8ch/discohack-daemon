use std::collections::HashMap;

use secret_service::{
    EncryptionType,
    blocking::{Item, SecretService},
};

use crate::auth::{AuthError, CredentialStore, StoredCredentials};

const ITEM_LABEL: &str = "discohack-daemon Yandex Disk OAuth";
const ATTR_SERVICE: &str = "service";
const ATTR_ACCOUNT: &str = "account";
const SERVICE_VALUE: &str = "ru.literallycats.daemon";
const ACCOUNT_VALUE: &str = "yandex-disk";
const CONTENT_TYPE: &str = "application/json";

#[derive(Default)]
pub struct SecretServiceStore;

impl SecretServiceStore {
    fn attributes() -> HashMap<&'static str, &'static str> {
        HashMap::from([(ATTR_SERVICE, SERVICE_VALUE), (ATTR_ACCOUNT, ACCOUNT_VALUE)])
    }

    fn connect() -> Result<SecretService<'static>, AuthError> {
        SecretService::connect(EncryptionType::Dh)
            .map_err(|err| AuthError::SecretStorage(err.to_string()))
    }

    fn first_item<'a>(items: &'a [Item<'a>]) -> Option<&'a Item<'a>> {
        items.first()
    }
}

impl CredentialStore for SecretServiceStore {
    fn load_credentials(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let secret_service = Self::connect()?;
        let search = secret_service
            .search_items(Self::attributes())
            .map_err(|err| AuthError::SecretStorage(err.to_string()))?;

        let item = Self::first_item(&search.unlocked)
            .or_else(|| Self::first_item(&search.locked))
            .map(|item| {
                if item
                    .is_locked()
                    .map_err(|err| AuthError::SecretStorage(err.to_string()))?
                {
                    item.unlock()
                        .map_err(|err| AuthError::SecretStorage(err.to_string()))?;
                }
                Ok::<_, AuthError>(item)
            })
            .transpose()?;

        let Some(item) = item else {
            return Ok(None);
        };

        let secret = item
            .get_secret()
            .map_err(|err| AuthError::SecretStorage(err.to_string()))?;
        let credentials = serde_json::from_slice::<StoredCredentials>(&secret)
            .map_err(|err| AuthError::SecretStorage(err.to_string()))?;
        Ok(Some(credentials))
    }

    fn save_credentials(&self, credentials: &StoredCredentials) -> Result<(), AuthError> {
        let secret_service = Self::connect()?;
        let collection = secret_service
            .get_any_collection()
            .map_err(|err| AuthError::SecretStorage(err.to_string()))?;
        let serialized = serde_json::to_vec(credentials)
            .map_err(|err| AuthError::SecretStorage(err.to_string()))?;

        collection
            .create_item(
                ITEM_LABEL,
                Self::attributes(),
                &serialized,
                true,
                CONTENT_TYPE,
            )
            .map_err(|err| AuthError::SecretStorage(err.to_string()))?;
        Ok(())
    }
}
