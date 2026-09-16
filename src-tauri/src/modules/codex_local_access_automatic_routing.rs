// Runtime-only API Service routing. Instance gateways keep their explicit routing configuration.

/// GPT / Codex 命名空间只保留官方推荐集（外加客户端内部需要的隐藏条目）。
/// 其它命名空间（如 DeepSeek）原样保留。
fn automatic_api_service_visible_model_ids(models: Vec<String>) -> Vec<String> {
    models
        .into_iter()
        .filter(|model| {
            let key = model.trim().to_ascii_lowercase();
            if key.is_empty() {
                return false;
            }
            if !key.starts_with("gpt-") && !key.starts_with("codex-") {
                return true;
            }
            key == CODEX_GPT_RESERVE_MODEL_ID
                || key == CODEX_AUTO_REVIEW_MODEL_ID
                || key.starts_with("gpt-image")
                || LOCAL_GATEWAY_VISIBLE_GPT_MODELS
                    .iter()
                    .any(|(model_id, _)| model_id.eq_ignore_ascii_case(&key))
        })
        .collect()
}

/// 账号自己的客户端可见模型槽位（客户端模型名 → 上游模型名）。
///
/// 与 DeepSeek 网关模式保持一致：以账号模型列表（`api_model_catalog` / provider 默认目录）
/// 作为可见清单，模型映射只用来决定每个模型的实际上游名；只有账号没有模型列表时才退回映射表。
fn automatic_api_service_account_model_slots(account: &CodexAccount) -> Vec<(String, String)> {
    let catalog = provider_gateway_models_for_account(account);
    if catalog.is_empty() {
        return account
            .api_model_mappings
            .iter()
            .map(|mapping| (mapping.client_model.clone(), mapping.upstream_model.clone()))
            .collect();
    }
    catalog
        .into_iter()
        .map(|model| {
            let upstream = account
                .api_model_mappings
                .iter()
                .find(|mapping| mapping.client_model.trim().eq_ignore_ascii_case(model.trim()))
                .map(|mapping| mapping.upstream_model.clone())
                .unwrap_or_else(|| model.clone());
            (model, upstream)
        })
        .collect()
}

/// 账号是否走 provider 路由（而不是原生账号池）。
///
/// Chat 协议账号一直如此；另外与 DeepSeek 网关模式一致，具备逐模型识图能力的账号也走
/// provider 路由，这样才能把带图片的请求自动转到识图模型。
fn automatic_api_service_provider_route_eligible(account: &CodexAccount) -> bool {
    account.is_api_key_auth()
        && (provider_gateway_wire_api_for_account(account) == "chat_completions"
            || !account.api_model_vision_support.is_empty())
}

/// 账号在自动路由里是否存在「图片自动转到识图模型」的兜底模型。
///
/// 判定与 sidecar 的 `providerGatewayVisionRoutingModel` 一致：显式识图路由优先，
/// 否则要求候选上游里恰好只有一个支持识图的模型。
fn automatic_api_service_vision_routing_model(
    account: &CodexAccount,
    upstream_models: &[String],
) -> Option<String> {
    let upstream_models = normalize_model_rule_list(upstream_models.to_vec());
    if let Some(explicit) = account
        .api_vision_routing_model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let matched = upstream_models
            .iter()
            .find(|model| model.eq_ignore_ascii_case(explicit))?;
        return Some(matched.clone());
    }
    let mut vision_model: Option<&String> = None;
    for model in &upstream_models {
        if !automatic_api_service_account_model_vision(account, model) {
            continue;
        }
        if vision_model.is_some_and(|current| !current.eq_ignore_ascii_case(model)) {
            return None;
        }
        vision_model = Some(model);
    }
    vision_model.cloned()
}

/// 单个模型的识图能力：用户显式开关优先，其次取官方 DeepSeek 默认值。
fn automatic_api_service_account_model_vision(account: &CodexAccount, model: &str) -> bool {
    let key = model.trim();
    if let Some(value) = account
        .api_model_vision_support
        .iter()
        .find(|(name, _)| name.trim().eq_ignore_ascii_case(key))
        .map(|(_, value)| *value)
    {
        return value;
    }
    is_official_deepseek_account(account)
        && codex_account::deepseek_model_effective_vision(account, key)
}

/// 单个模型在 provider 网关里的识图能力：模型级开关优先，其次取网关默认值。
fn provider_gateway_model_vision(
    gateway: &CodexLocalAccessProviderGateway,
    model: &str,
) -> bool {
    gateway
        .model_capabilities
        .iter()
        .find(|(name, _)| name.trim().eq_ignore_ascii_case(model.trim()))
        .map(|(_, capability)| capability.supports_vision)
        .unwrap_or(gateway.supports_vision)
}

/// provider 网关是否具备「图片自动转到识图模型」的兜底能力。
///
/// 判定与 sidecar 的 `providerGatewayVisionRoutingModel` 保持一致：显式识图路由优先，
/// 否则要求候选上游里恰好只有一个支持识图的模型。
fn provider_gateway_has_vision_support(gateway: &CodexLocalAccessProviderGateway) -> bool {
    let upstream_models = normalize_model_rule_list(gateway.upstream_models.clone());
    if let Some(explicit) = gateway
        .vision_routing_model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let matched = if upstream_models.is_empty() {
            Some(explicit.to_string())
        } else {
            upstream_models
                .iter()
                .find(|model| model.eq_ignore_ascii_case(explicit))
                .cloned()
        };
        return matched.is_some_and(|model| provider_gateway_model_vision(gateway, &model));
    }
    upstream_models
        .iter()
        .filter(|model| provider_gateway_model_vision(gateway, model))
        .count()
        == 1
}

fn automatic_api_service_pool_model_ids(accounts: &[CodexAccount], fallback: Vec<String>) -> Vec<String> {
    // Keep the existing empty-pool catalog so configuration remains possible before adding accounts.
    if accounts.is_empty() { return fallback; }
    let mut models = Vec::new();
    for account in accounts {
        if !account.is_api_key_auth() {
            models.extend(fallback.iter().cloned());
            continue;
        }
        let slots = automatic_api_service_account_model_slots(account);
        if slots.is_empty() {
            if provider_gateway_wire_api_for_account(account) == "responses" {
                models.extend(fallback.iter().cloned());
            }
            continue;
        }
        // 客户端可请求的是「客户端模型名」；上游名只用于转发，不进入可见清单。
        models.extend(slots.into_iter().map(|(client, _)| client));
    }
    automatic_api_service_visible_model_ids(normalize_model_rule_list(models))
}

fn automatic_api_service_route_namespace(account_id: &str) -> String {
    sidecar_stable_id("api", &[account_id]).replace(':', "-")
}

fn automatic_api_service_route_models(
    collection: &CodexLocalAccessCollection,
    account: &CodexAccount,
    fallback_models: &[String],
) -> Vec<Value> {
    let mut slots = automatic_api_service_account_model_slots(account);
    if slots.is_empty()
        && (!account.is_api_key_auth()
            || provider_gateway_wire_api_for_account(account) == "responses")
    {
        slots = fallback_models
            .iter()
            .map(|model| (model.clone(), model.clone()))
            .collect();
    }
    let account_excluded = collection.account_model_rules.iter()
        .find(|rule| rule.account_id == account.id)
        .map(|rule| rule.excluded_models.as_slice()).unwrap_or_default();
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for (client, upstream) in slots {
        let client = client.trim();
        let upstream = upstream.trim();
        if client.is_empty() || upstream.is_empty()
            || model_matches_any_rule(client, account_excluded)
            || model_matches_any_rule(upstream, account_excluded) {
            continue;
        }
        for alias in apply_model_aliases_to_ids(vec![client.to_string()], &collection.model_aliases) {
            if !model_matches_any_rule(&alias, &collection.excluded_models)
                && !model_matches_any_rule(&alias, account_excluded)
                && seen.insert(alias.to_ascii_lowercase()) {
                // 显示名与推理档位沿用账号官方模板（例如 DeepSeek 官方 models.json），
                // 这样客户端通过网关拿到的名称与档位和 DeepSeek 网关模式完全一致。
                let template = account_model_template(account, &alias)
                    .or_else(|| account_model_template(account, upstream));
                let display_name = codex_account::provider_model_display_name(&alias);
                let mut entry = json!({
                    "clientModel": alias,
                    "upstreamModel": upstream,
                });
                if display_name.trim() != alias.trim() {
                    entry["displayName"] = json!(display_name);
                }
                if let Some(template) = template {
                    if let Some(levels) = template.get("supported_reasoning_levels") {
                        entry["reasoningLevels"] = levels.clone();
                    }
                    if let Some(default_level) = template.get("default_reasoning_level") {
                        entry["defaultReasoningLevel"] = default_level.clone();
                    }
                }
                models.push(entry);
            }
        }
    }
    models
}

/// DeepSeek 模型在客户端模型列表里的默认推理档位。
///
/// 官方 DeepSeek 模板声明为 `high`，但 Cockpit 的 API 服务默认选最高档：DeepSeek 官方目录
/// 已声明支持 `max`，接管 profile 时也会补齐客户端的 `max` 档位开关。该值只决定
/// 「尚未选择过档位」或「当前档位对该模型不可用」时的回落值，不会覆盖用户已保存的档位。
const DEEPSEEK_DEFAULT_REASONING_EFFORT: &str = "max";

/// 账号模型的官方模板（目前覆盖 DeepSeek 官方目录）。
fn account_model_template(account: &CodexAccount, model: &str) -> Option<Value> {
    let model = model.trim();
    if model.is_empty() || !is_official_deepseek_account(account) {
        return None;
    }
    let catalog = codex_account::deepseek_official_catalog_json_for_account(account).ok()?;
    let catalog: Value = serde_json::from_str(&catalog).ok()?;
    let mut entry = catalog
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|entry| {
            entry
                .get("slug")
                .and_then(Value::as_str)
                .is_some_and(|slug| slug.eq_ignore_ascii_case(model))
        })
        .cloned()?;
    // 其余字段沿用官方模板，只把默认档位提升为最高档。
    entry["default_reasoning_level"] = json!(DEEPSEEK_DEFAULT_REASONING_EFFORT);
    Some(entry)
}

fn automatic_api_service_model_routing_value(
    collection: &CodexLocalAccessCollection,
    account_ids: &[String],
    accounts: &HashMap<String, CodexAccount>,
) -> Value {
    let mut fallback = api_service_supported_codex_model_ids();
    if !fallback.iter().any(|model| model.eq_ignore_ascii_case(CODEX_GPT_RESERVE_MODEL_ID)) {
        fallback.push(CODEX_GPT_RESERVE_MODEL_ID.to_string());
    }
    let mut native_models = Vec::new();
    let mut seen_native = HashSet::new();
    let mut routes = Vec::new();
    for account_id in normalize_account_id_list(account_ids.to_vec()) {
        let Some(account) = accounts.get(&account_id) else { continue; };
        let models = automatic_api_service_route_models(collection, account, &fallback);
        if !automatic_api_service_provider_route_eligible(account) {
            for model in models {
                let client = model["clientModel"].as_str().unwrap_or_default();
                if seen_native.insert(client.to_ascii_lowercase()) {
                    native_models.push(client.to_string());
                }
            }
            continue;
        }
        if models.is_empty() { continue; }
        let Ok(mut gateway) = provider_gateway_for_account(account) else { continue; };
        let Some(base_url) = resolve_sidecar_upstream_base_url(account, collection) else { continue; };
        gateway.base_url = base_url;
        // Explicit mappings can contain upstream names absent from the discovered catalog.
        gateway.upstream_models = normalize_model_rule_list(models.iter()
            .filter_map(|model| model["upstreamModel"].as_str().map(str::to_string)).collect());
        gateway.upstream_model = gateway.upstream_models.first().cloned().unwrap_or_default();
        // 与 DeepSeek 网关模式一致：带图片的文本模型请求自动转到识图模型。
        gateway.vision_routing_model =
            automatic_api_service_vision_routing_model(account, &gateway.upstream_models);
        routes.push(json!({
            "id": format!("auto-{}", account.id),
            "namespace": automatic_api_service_route_namespace(&account.id),
            "providerAccountId": account.id,
            "providerGateway": gateway,
            "models": models,
        }));
    }
    let native_models = automatic_api_service_visible_model_ids(native_models);
    json!({
        "automatic": true,
        "nativeModels": native_models,
        // 不展示但仍可路由的历史模型（唤醒预设、兼容模型）；客户端选择器看不到它们。
        "routableModels": api_service_routable_codex_model_ids(),
        "defaultRoute": "oauth",
        "failurePolicy": "strict",
        "routes": routes,
    })
}

fn apply_automatic_api_service_model_routing(
    values: &mut [Value],
    collection: &CodexLocalAccessCollection,
    accounts: &HashMap<String, CodexAccount>,
) {
    for value in values {
        if value.get("internal").and_then(Value::as_bool) == Some(true) {
            // 宿主内部请求（唤醒、鹈鹕测试）固定落到指定账号，因此只保留原生路由：
            // 历史模型仍然可用，且不会把内部请求转交到其它账号。
            let routable = api_service_routable_codex_model_ids();
            value["modelRouting"] = json!({
                "automatic": true,
                "nativeModels": routable,
                "routableModels": routable,
                "defaultRoute": "oauth",
                "failurePolicy": "strict",
                "routes": [],
            });
            continue;
        }
        if value
            .get("providerGateway")
            .is_some_and(|gateway| !gateway.is_null())
            || value.get("modelRouting").is_some_and(|routing| !routing.is_null()) {
            continue;
        }
        let account_ids = value.get("accountIds").and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_default();
        let routing = automatic_api_service_model_routing_value(collection, &account_ids, accounts);
        if routing["routes"].as_array().is_some_and(|routes| !routes.is_empty()) {
            value["responsesWebsockets"] = Value::Bool(false);
        }
        value["modelRouting"] = routing;
    }
}

/// 单个账号自带的模型 ID（客户端模型名）。
///
/// 官方模型清单由调用方单独提供，因此这里传空兜底清单：OAuth 等原生账号不会重复加入
/// 官方模型，只有 API Key / 供应商账号自己的模型目录与账号级映射会返回。
fn automatic_api_service_account_model_entries(
    collection: &CodexLocalAccessCollection,
    account: &CodexAccount,
) -> Vec<(String, bool)> {
    let slots = automatic_api_service_account_model_slots(account);
    let upstream_models = slots
        .iter()
        .map(|(_, upstream)| upstream.clone())
        .collect::<Vec<_>>();
    // provider 路由账号：只要存在识图兜底模型，客户端就可以对任意模型发图片（与 DeepSeek 网关模式一致）。
    let routed_vision = automatic_api_service_provider_route_eligible(account)
        && automatic_api_service_vision_routing_model(account, &upstream_models).is_some();
    automatic_api_service_route_models(collection, account, &[])
        .into_iter()
        .filter_map(|model| {
            let client_model = model.get("clientModel").and_then(Value::as_str)?.to_string();
            let image_capable = routed_vision
                || automatic_api_service_account_model_vision(account, &client_model);
            Some((client_model, image_capable))
        })
        .collect()
}

/// API 服务 profile 的客户端模型目录需要补入的模型。
///
/// 客户端模型选择器读取 profile 的模型目录，因此集合账号自带的模型（例如 DeepSeek）
/// 必须写进目录才能在界面上显示与切换；这里只返回账号/网关模型，官方清单由调用方维护。
fn automatic_api_service_profile_extra_models(
    collection: &CodexLocalAccessCollection,
    api_key: &ResolvedLocalApiKey,
    accounts: &[CodexAccount],
) -> Vec<(String, bool)> {
    let scoped_ids = scoped_collection_account_ids(collection, api_key);
    let mut models = Vec::new();
    for account in accounts {
        if !scoped_ids.iter().any(|id| id == &account.id)
            || !is_local_access_eligible_account(account, collection.restrict_free_accounts)
        {
            continue;
        }
        models.extend(automatic_api_service_account_model_entries(collection, account));
    }
    // 该 API Key 自己的供应商网关模型同样要在客户端可选。
    if let Some(gateway) = api_key.provider_gateway.as_ref() {
        let image_capable = provider_gateway_has_vision_support(gateway);
        models.extend(
            apply_model_aliases_to_ids(gateway.upstream_models.clone(), &collection.model_aliases)
                .into_iter()
                .map(|model| (model, image_capable)),
        );
    }
    // GPT / Codex 命名空间只保留官方推荐集：账号映射里的 shell 别名（例如 DeepSeek 的
    // `gpt-5.4-mini`）仍然可以路由，但不再额外出现在客户端选择器里。
    let mut seen = HashSet::new();
    models.retain(|(model, _)| {
        let key = model.trim().to_ascii_lowercase();
        !model.trim().is_empty()
            && !key.starts_with("gpt-")
            && !key.starts_with("codex-")
            && !model_matches_any_rule(model, &collection.excluded_models)
            && (api_key.allowed_models.is_empty()
                || model_matches_any_rule(model, &api_key.allowed_models))
            && !model_matches_any_rule(model, &api_key.excluded_models)
            && seen.insert(key)
    });
    models
}
