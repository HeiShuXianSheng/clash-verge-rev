use std::sync::atomic::{AtomicBool, Ordering};

use clash_verge_logging::{Type, logging};

use crate::{
    config::{Config, profiles},
    core::{
        handle,
        notify::{Refresh, announce},
        tray::Tray,
    },
};

/// Used when `auto_select_group` is unset or names a group the core does not expose.
const DEFAULT_GROUP: &str = "Proxies";
const DEFAULT_TEST_URL: &str = "http://cp.cloudflare.com/generate_204";
const TEST_TIMEOUT_MS: u32 = 5000;

/// Concurrent runs would overlap their group delay tests and corrupt each other's results.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Tests every member of the target group and selects the reachable leaf with the lowest delay.
///
/// `delay_group` drops the group's fixed selection, so a run without a reachable leaf restores the
/// node selected before the test instead of leaving the group on the core's own choice.
pub async fn speedtest_and_select() {
    if RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        logging!(debug, Type::ProxyMode, "speedtest: skipped, previous run in progress");
        return;
    }

    select_fastest().await;
    RUNNING.store(false, Ordering::Release);
}

async fn select_fastest() {
    let mihomo = handle::Handle::mihomo();
    let Ok(proxies) = mihomo.get_proxies().await else {
        logging!(warn, Type::ProxyMode, "speedtest: failed to load proxies");
        return;
    };

    let configured = Config::verge()
        .await
        .latest_arc()
        .auto_select_group
        .as_deref()
        .map(str::to_owned);
    let group_name = configured
        .filter(|name| !name.is_empty() && proxies.proxies.contains_key(name))
        .unwrap_or_else(|| DEFAULT_GROUP.to_owned());

    let Some(group) = proxies.proxies.get(&group_name) else {
        logging!(warn, Type::ProxyMode, "speedtest: group {group_name} not found");
        return;
    };
    if group.all.is_none() {
        logging!(warn, Type::ProxyMode, "speedtest: {group_name} is not a proxy group");
        return;
    }

    let previous = group.now.clone();
    // Selector groups report an empty test_url, which mihomo treats as "test nothing".
    let test_url = group
        .test_url
        .clone()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_TEST_URL.to_owned());

    logging!(info, Type::ProxyMode, "speedtest: testing {group_name} members");

    let delays = match mihomo.delay_group(&group_name, &test_url, TEST_TIMEOUT_MS).await {
        Ok(delays) => delays,
        Err(err) => {
            logging!(
                warn,
                Type::ProxyMode,
                "speedtest: delay test failed for {group_name} using {test_url}: {err:#}"
            );
            return;
        }
    };

    // Nested groups report their own delay entry; only leaf nodes may be selected.
    let best = delays
        .into_iter()
        .filter(|(name, delay)| {
            *delay > 0
                && !is_informational_node(name)
                && proxies.proxies.get(name).is_some_and(|proxy| proxy.all.is_none())
        })
        .min_by_key(|(_, delay)| *delay);

    let Some((node, delay)) = best else {
        logging!(warn, Type::ProxyMode, "speedtest: no reachable node in {group_name}");
        restore_selection(&group_name, previous.as_deref()).await;
        return;
    };

    if let Err(err) = mihomo.select_node_for_group(&group_name, &node).await {
        logging!(
            error,
            Type::ProxyMode,
            "speedtest: failed to select {node} in {group_name}: {err:#}"
        );
        restore_selection(&group_name, previous.as_deref()).await;
        return;
    }

    if let Err(err) = profiles::record_selected_node(&group_name, &node).await {
        logging!(
            warn,
            Type::ProxyMode,
            "speedtest: failed to persist {group_name} -> {node}: {err:#}"
        );
    }

    announce(Refresh::Clash);
    if let Err(err) = Tray::global().update_menu().await {
        logging!(warn, Type::Tray, "speedtest: failed to refresh tray menu: {err:#}");
    }

    logging!(info, Type::ProxyMode, "speedtest: {group_name} -> {node} ({delay}ms)");
}

/// Subscriptions often expose traffic and expiry banners as proxy entries; the hotkey must not
/// route traffic through one of them.
fn is_informational_node(name: &str) -> bool {
    const MARKERS: [&str; 8] = [
        "traffic",
        "expire",
        "remaining",
        "expiry",
        "剩余",
        "到期",
        "套餐",
        "官网",
    ];
    let name = name.to_ascii_lowercase();
    MARKERS.into_iter().any(|marker| name.contains(marker))
}

async fn restore_selection(group_name: &str, previous: Option<&str>) {
    let Some(previous) = previous else {
        return;
    };
    if let Err(err) = handle::Handle::mihomo()
        .select_node_for_group(group_name, previous)
        .await
    {
        logging!(
            error,
            Type::ProxyMode,
            "speedtest: failed to restore {previous} in {group_name}: {err:#}"
        );
    }
}
