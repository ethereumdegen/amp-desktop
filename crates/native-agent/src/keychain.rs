//! Status checks must not decrypt credentials or display permission UI.
//! Actual secret reads stay in the session credential caches.
pub(crate) fn contains(service: &str, account: &str) -> bool {
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
    if service.is_empty() || account.is_empty() {
        return false;
    }
    ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(service)
        .account(account)
        .limit(Limit::Max(1))
        .load_refs(true)
        .load_data(false)
        .skip_authenticated_items(true)
        .search()
        .is_ok_and(|items| !items.is_empty())
}
