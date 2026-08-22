use gas_auction_bot::Settings;

#[test]
fn settings_load_from_project_config() {
    let settings = Settings::load().expect("settings should load from config.toml");
    assert_eq!(settings.network.chain_id, 1);
    assert!(settings.gas.max_gas_price_gwei > settings.gas.min_gas_price_gwei);
    assert!(settings.safety.circuit_breaker_enabled);
}
