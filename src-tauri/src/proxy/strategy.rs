use crate::db::{self, Aggregator, CodingPlan};
use rusqlite::Connection;

/// 一次转发选中的计划
#[derive(Debug, Clone)]
pub struct StrategyPick {
    pub binding_id: i64,
    pub plan: CodingPlan,
}

/// 阈值轮转策略：
/// 1. 手动指定的当前计划（aggregators.current_plan_id 非空）有效
///    （已绑定且启用且已用 token < 阈值）-> 固定走该计划，不触发轮转
/// 2. 指定的计划失效（达阈值/禁用/解绑）-> 清除指定，恢复自动轮转
/// 3. 按绑定顺序取第一个「启用中 且 已用 token < 阈值」的计划
/// 4. 所有计划都达到阈值 -> 该聚合器全部绑定的用量清零，回到第一个重新计数
/// 5. 没有任何启用的计划 -> None（由调用方返回 503）
fn pick_threshold_rotation(
    conn: &Connection,
    agg: &Aggregator,
) -> rusqlite::Result<Option<StrategyPick>> {
    let bindings = db::list_bindings(conn, agg.id)?;
    let active: Vec<_> = bindings.iter().filter(|b| b.plan.enabled).collect();
    if active.is_empty() {
        return Ok(None);
    }

    if let Some(pid) = agg.current_plan_id {
        if let Some(b) = active.iter().find(|b| b.plan.id == pid) {
            if b.used_tokens < agg.token_threshold {
                return Ok(Some(StrategyPick {
                    binding_id: b.binding_id,
                    plan: b.plan.clone(),
                }));
            }
        }
        // 手动指定的计划已失效（达阈值/禁用/解绑）：清除指定，恢复自动轮转
        db::clear_current_plan(conn, agg.id)?;
    }

    for b in &active {
        if b.used_tokens < agg.token_threshold {
            return Ok(Some(StrategyPick {
                binding_id: b.binding_id,
                plan: b.plan.clone(),
            }));
        }
    }

    // 全部超额：清零并回绕到第一个
    db::reset_binding_usage(conn, agg.id)?;
    let first = active[0];
    Ok(Some(StrategyPick {
        binding_id: first.binding_id,
        plan: first.plan.clone(),
    }))
}

/// 模型匹配策略：按请求携带的模型名路由到配置了该「支持模型」的计划。
/// 「当前计划」是持久化状态（aggregators.current_plan_id），粘性持有：
/// 1. 匹配集非空 且 当前计划在其中 -> 路由不变（即使别的匹配计划用量更少）
/// 2. 匹配集非空 且 当前不在其中 -> 切到匹配集中绑定累计 token 最少者
///    （同量按绑定顺序取先者），并把新计划持久化为当前计划
/// 3. 无匹配（含请求未携带模型）-> 路由到当前计划
/// 4. 当前计划缺失/失效（未绑定、被禁用）-> 回退第一个启用绑定并持久化
/// 5. 没有任何启用的计划 -> None（由调用方返回 503）
/// token 阈值在本策略下不参与路由；used_tokens 仅用于展示与「最少用量」比较。
fn pick_model_match(
    conn: &Connection,
    agg: &Aggregator,
    model: &str,
) -> rusqlite::Result<Option<StrategyPick>> {
    let bindings = db::list_bindings(conn, agg.id)?;
    let active: Vec<_> = bindings.iter().filter(|b| b.plan.enabled).collect();
    if active.is_empty() {
        return Ok(None);
    }

    let current = active
        .iter()
        .find(|b| Some(b.plan.id) == agg.current_plan_id);

    if !model.is_empty() {
        let matches: Vec<_> = active
            .iter()
            .filter(|b| b.plan.models.iter().any(|m| m == model))
            .collect();
        if !matches.is_empty() {
            if let Some(cur) = current {
                if matches.iter().any(|m| m.plan.id == cur.plan.id) {
                    return Ok(Some(StrategyPick {
                        binding_id: cur.binding_id,
                        plan: cur.plan.clone(),
                    }));
                }
            }
            // min_by_key 同量取先（迭代序即绑定顺序）
            let best = matches.into_iter().min_by_key(|b| b.used_tokens).unwrap();
            db::set_current_plan(conn, agg.id, best.plan.id)?;
            return Ok(Some(StrategyPick {
                binding_id: best.binding_id,
                plan: best.plan.clone(),
            }));
        }
    }

    if let Some(cur) = current {
        return Ok(Some(StrategyPick {
            binding_id: cur.binding_id,
            plan: cur.plan.clone(),
        }));
    }
    // 当前计划缺失/失效：回退到第一个启用绑定并固化，避免每次请求都漂移
    let first = active[0];
    db::set_current_plan(conn, agg.id, first.plan.id)?;
    Ok(Some(StrategyPick {
        binding_id: first.binding_id,
        plan: first.plan.clone(),
    }))
}

/// 按聚合器策略选择转发的计划。model 为请求体携带的模型名（阈值轮转忽略之）；
/// 未知策略值兜底为阈值轮转（老库数据不至于直接 503）。
pub fn pick_plan(
    conn: &Connection,
    agg: &Aggregator,
    model: &str,
) -> rusqlite::Result<Option<StrategyPick>> {
    match agg.strategy.as_str() {
        db::STRATEGY_MODEL_MATCH => pick_model_match(conn, agg, model),
        _ => pick_threshold_rotation(conn, agg),
    }
}

/// 无副作用地推导「下一个将被选中的计划」：选择规则与 pick_plan 一致，
/// 但绝不触发清零回绕/当前计划写入。供列表/详情查询派生 current_plan_id，
/// 修改轮转规则时两处必须同步。
pub fn peek_plan(conn: &Connection, agg: &Aggregator) -> rusqlite::Result<Option<i64>> {
    match agg.strategy.as_str() {
        db::STRATEGY_MODEL_MATCH => peek_model_match(conn, agg),
        _ => peek_threshold_rotation(conn, agg),
    }
}

fn peek_threshold_rotation(conn: &Connection, agg: &Aggregator) -> rusqlite::Result<Option<i64>> {
    let bindings = db::list_bindings(conn, agg.id)?;
    let active: Vec<_> = bindings.iter().filter(|b| b.plan.enabled).collect();
    if active.is_empty() {
        return Ok(None);
    }

    // 手动指定且有效：展示指定值（失效时 pick 会先清除再轮转，这里展示轮转结果）
    if let Some(pid) = agg.current_plan_id {
        if let Some(b) = active.iter().find(|b| b.plan.id == pid) {
            if b.used_tokens < agg.token_threshold {
                return Ok(Some(b.plan.id));
            }
        }
    }

    for b in &active {
        if b.used_tokens < agg.token_threshold {
            return Ok(Some(b.plan.id));
        }
    }
    // 全部超额：回绕后用的也是第一个（不清零，用量仍由 pick_plan 在真正转发时重置）
    Ok(Some(active[0].plan.id))
}

fn peek_model_match(conn: &Connection, agg: &Aggregator) -> rusqlite::Result<Option<i64>> {
    let bindings = db::list_bindings(conn, agg.id)?;
    let active: Vec<_> = bindings.iter().filter(|b| b.plan.enabled).collect();
    if active.is_empty() {
        return Ok(None);
    }
    // 当前计划有效则展示之，否则展示回退目标（不落库，落库只在 pick_plan）
    if active
        .iter()
        .any(|b| Some(b.plan.id) == agg.current_plan_id)
    {
        return Ok(agg.current_plan_id);
    }
    Ok(Some(active[0].plan.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db as db_mod;

    fn setup() -> (Connection, Aggregator, i64, i64) {
        let conn = db_mod::init_db(":memory:").unwrap();
        let p1 = db_mod::create_plan(&conn, "p1", "https://a", "t1", "", &[]).unwrap();
        let p2 = db_mod::create_plan(&conn, "p2", "https://b", "t2", "", &[]).unwrap();
        let agg = db_mod::create_aggregator(
            &conn,
            "g",
            8300,
            "cpm",
            100,
            db_mod::STRATEGY_THRESHOLD_ROTATION,
        )
        .unwrap();
        db_mod::set_aggregator_plans(&conn, agg.id, &[p1.id, p2.id]).unwrap();
        (conn, agg, p1.id, p2.id)
    }

    #[test]
    fn picks_first_then_next() {
        let (conn, agg, p1, p2) = setup();
        let pick = pick_plan(&conn, &agg, "").unwrap().unwrap();
        assert_eq!(pick.plan.id, p1);

        // p1 达到阈值 -> 切到 p2
        let b = db_mod::list_bindings(&conn, agg.id).unwrap().remove(0);
        db_mod::add_binding_usage(&conn, b.binding_id, 100).unwrap();
        let pick = pick_plan(&conn, &agg, "").unwrap().unwrap();
        assert_eq!(pick.plan.id, p2);

        // 唯一启用的 p1 已超额：清零回绕继续用 p1（符合"全部超额->清零重来"语义）
        db_mod::update_plan(&conn, p2, "p2", "https://b", "t2", "", false, &[]).unwrap();
        let pick = pick_plan(&conn, &agg, "").unwrap().unwrap();
        assert_eq!(pick.plan.id, p1);

        // 全部禁用 -> 无可用计划
        db_mod::update_plan(&conn, p1, "p1", "https://a", "t1", "", false, &[]).unwrap();
        assert!(pick_plan(&conn, &agg, "").unwrap().is_none());
    }

    #[test]
    fn all_exhausted_resets_and_wraps() {
        let (conn, agg, p1, p2) = setup();
        for b in db_mod::list_bindings(&conn, agg.id).unwrap() {
            db_mod::add_binding_usage(&conn, b.binding_id, 200).unwrap();
        }
        let pick = pick_plan(&conn, &agg, "").unwrap().unwrap();
        assert_eq!(pick.plan.id, p1); // 回绕到第一个

        // 所有绑定已被清零
        let bs = db_mod::list_bindings(&conn, agg.id).unwrap();
        assert!(bs.iter().all(|b| b.used_tokens == 0));
        let _ = p2;
    }

    #[test]
    fn skips_disabled() {
        let (conn, agg, p1, p2) = setup();
        db_mod::update_plan(&conn, p1, "p1", "https://a", "t1", "", false, &[]).unwrap();
        let pick = pick_plan(&conn, &agg, "").unwrap().unwrap();
        assert_eq!(pick.plan.id, p2);
    }

    #[test]
    fn peek_matches_pick_without_side_effects() {
        let (conn, agg, p1, p2) = setup();

        // 首个未超额：两者一致
        assert_eq!(peek_plan(&conn, &agg).unwrap(), Some(p1));
        assert_eq!(pick_plan(&conn, &agg, "").unwrap().unwrap().plan.id, p1);

        // p1 超额 -> 一致切到 p2；跳过禁用计划与 pick 行为相同
        let b = db_mod::list_bindings(&conn, agg.id).unwrap().remove(0);
        db_mod::add_binding_usage(&conn, b.binding_id, 100).unwrap();
        assert_eq!(peek_plan(&conn, &agg).unwrap(), Some(p2));
        db_mod::update_plan(&conn, p1, "p1", "https://a", "t1", "", false, &[]).unwrap();
        assert_eq!(peek_plan(&conn, &agg).unwrap(), Some(p2));

        // 全部禁用 -> None（与 pick 的 503 语义对应）
        db_mod::update_plan(&conn, p2, "p2", "https://b", "t2", "", false, &[]).unwrap();
        assert_eq!(peek_plan(&conn, &agg).unwrap(), None);
    }

    #[test]
    fn peek_all_exhausted_wraps_without_reset() {
        let (conn, agg, p1, _p2) = setup();
        for b in db_mod::list_bindings(&conn, agg.id).unwrap() {
            db_mod::add_binding_usage(&conn, b.binding_id, 200).unwrap();
        }

        // 回绕到第一个，但不清零（清零只应发生在真正转发的 pick_plan 里）
        assert_eq!(peek_plan(&conn, &agg).unwrap(), Some(p1));
        let bs = db_mod::list_bindings(&conn, agg.id).unwrap();
        assert!(bs.iter().all(|b| b.used_tokens > 0), "peek 不应清零用量");

        // 随后真正 pick 时：清零并选中同一个计划
        let pick = pick_plan(&conn, &agg, "").unwrap().unwrap();
        assert_eq!(pick.plan.id, p1);
        let bs = db_mod::list_bindings(&conn, agg.id).unwrap();
        assert!(bs.iter().all(|b| b.used_tokens == 0));
    }

    #[test]
    fn peek_none_when_no_bindings() {
        let conn = db_mod::init_db(":memory:").unwrap();
        let agg = db_mod::create_aggregator(
            &conn,
            "g",
            8300,
            "cpm",
            100,
            db_mod::STRATEGY_THRESHOLD_ROTATION,
        )
        .unwrap();
        assert_eq!(peek_plan(&conn, &agg).unwrap(), None);
    }

    // -----------------------------------------------------------------------
    // 阈值轮转：手动固定当前计划
    // -----------------------------------------------------------------------

    #[test]
    fn threshold_manual_pin_wins_over_rotation() {
        // 固定 p2：即使按顺序该轮到 p1，也优先走 p2
        let (conn, agg, p1, p2) = setup();
        db_mod::set_current_plan(&conn, agg.id, p2).unwrap();
        let agg = fresh(&conn, agg.id);
        assert_eq!(pick_plan(&conn, &agg, "").unwrap().unwrap().plan.id, p2);
        assert_eq!(peek_plan(&conn, &agg).unwrap(), Some(p2));
        // 有效固定不产生任何副作用：指定保持、用量不动
        assert_eq!(current(&conn, agg.id), Some(p2));
        let bs = db_mod::list_bindings(&conn, agg.id).unwrap();
        assert!(bs.iter().all(|b| b.used_tokens == 0));
        let _ = p1;
    }

    #[test]
    fn threshold_pin_cleared_when_exhausted() {
        // 固定的 p2 达到阈值 -> 清除指定，轮转到未超额的 p1
        let (conn, agg, p1, p2) = setup();
        let b2 = db_mod::list_bindings(&conn, agg.id).unwrap().remove(1);
        db_mod::add_binding_usage(&conn, b2.binding_id, 100).unwrap();
        db_mod::set_current_plan(&conn, agg.id, p2).unwrap();
        let agg = fresh(&conn, agg.id);
        // peek 展示轮转结果 p1（指定已失效），且自身不清除
        assert_eq!(peek_plan(&conn, &agg).unwrap(), Some(p1));
        assert_eq!(pick_plan(&conn, &agg, "").unwrap().unwrap().plan.id, p1);
        assert_eq!(current(&conn, agg.id), None, "失效的指定应被清除");
    }

    #[test]
    fn threshold_pin_cleared_when_disabled_or_unbound() {
        let (conn, agg, p1, p2) = setup();
        // 指定的计划被禁用 -> 清除并轮转
        db_mod::set_current_plan(&conn, agg.id, p2).unwrap();
        db_mod::update_plan(&conn, p2, "p2", "https://b", "t2", "", false, &[]).unwrap();
        let agg = fresh(&conn, agg.id);
        assert_eq!(pick_plan(&conn, &agg, "").unwrap().unwrap().plan.id, p1);
        assert_eq!(current(&conn, agg.id), None, "禁用的指定计划应被清除");

        // 指定的计划被解绑 -> 同样清除并轮转
        db_mod::set_current_plan(&conn, agg.id, p2).unwrap();
        db_mod::set_aggregator_plans(&conn, agg.id, &[p1]).unwrap();
        let agg = fresh(&conn, agg.id);
        assert_eq!(pick_plan(&conn, &agg, "").unwrap().unwrap().plan.id, p1);
        assert_eq!(current(&conn, agg.id), None);
    }

    #[test]
    fn threshold_pin_exhausted_with_all_wraps() {
        // 固定的 p1 也超额（p2 早已超额）-> 清除指定 + 清零回绕到第一个
        let (conn, agg, p1, _p2) = setup();
        for b in db_mod::list_bindings(&conn, agg.id).unwrap() {
            db_mod::add_binding_usage(&conn, b.binding_id, 200).unwrap();
        }
        db_mod::set_current_plan(&conn, agg.id, p1).unwrap();
        let agg = fresh(&conn, agg.id);
        let pick = pick_plan(&conn, &agg, "").unwrap().unwrap();
        assert_eq!(pick.plan.id, p1);
        assert_eq!(current(&conn, agg.id), None, "回绕后应恢复自动轮转");
        let bs = db_mod::list_bindings(&conn, agg.id).unwrap();
        assert!(bs.iter().all(|b| b.used_tokens == 0));
    }

    // -----------------------------------------------------------------------
    // 模型匹配策略
    // -----------------------------------------------------------------------

    /// 建 model_match 聚合器，plans 按 models 列表依次创建（空切片 = 不配置）
    fn setup_model(models: &[&[&str]]) -> (Connection, Aggregator, Vec<i64>) {
        let conn = db_mod::init_db(":memory:").unwrap();
        let mut ids = Vec::new();
        for (i, ms) in models.iter().enumerate() {
            let owned: Vec<String> = ms.iter().map(|s| s.to_string()).collect();
            let p = db_mod::create_plan(
                &conn,
                &format!("p{i}"),
                "https://a",
                &format!("t{i}"),
                "",
                &owned,
            )
            .unwrap();
            ids.push(p.id);
        }
        let agg = db_mod::create_aggregator(
            &conn,
            "g",
            8300,
            "cpm",
            1_000_000,
            db_mod::STRATEGY_MODEL_MATCH,
        )
        .unwrap();
        db_mod::set_aggregator_plans(&conn, agg.id, &ids).unwrap();
        (conn, agg, ids)
    }

    fn current(conn: &Connection, agg_id: i64) -> Option<i64> {
        db_mod::get_aggregator(conn, agg_id)
            .unwrap()
            .unwrap()
            .current_plan_id
    }

    /// 重新读取聚合器：模拟 handler 每请求从 DB 重读配置（pick 依赖新鲜的 current_plan_id）
    fn fresh(conn: &Connection, agg_id: i64) -> Aggregator {
        db_mod::get_aggregator(conn, agg_id).unwrap().unwrap()
    }

    #[test]
    fn model_match_current_in_matches_stays() {
        // p1/p2 都配置 m1 且 p2 用量更少；当前为 p1 -> 粘在 p1，不切换
        let (conn, agg, ids) = setup_model(&[&["m1"], &["m1"]]);
        db_mod::set_current_plan(&conn, agg.id, ids[0]).unwrap();
        let bs = db_mod::list_bindings(&conn, agg.id).unwrap();
        db_mod::add_binding_usage(&conn, bs[1].binding_id, 500).unwrap();

        let agg = fresh(&conn, agg.id);
        let pick = pick_plan(&conn, &agg, "m1").unwrap().unwrap();
        assert_eq!(pick.plan.id, ids[0], "当前计划在匹配集中应保持不变");
        assert_eq!(current(&conn, agg.id), Some(ids[0]));
    }

    #[test]
    fn model_match_switches_and_persists() {
        // 当前 p1 未配置模型，请求 m2 只被 p2 支持 -> 切到 p2 并持久化
        let (conn, agg, ids) = setup_model(&[&[], &["m2"]]);
        db_mod::set_current_plan(&conn, agg.id, ids[0]).unwrap();

        let agg = fresh(&conn, agg.id);
        let pick = pick_plan(&conn, &agg, "m2").unwrap().unwrap();
        assert_eq!(pick.plan.id, ids[1], "当前不匹配时应切到匹配计划");
        assert_eq!(
            current(&conn, agg.id),
            Some(ids[1]),
            "切换应持久化为新当前计划"
        );

        // 后续同模型请求命中粘性规则，仍是 p2；peek 与之一致
        let agg = fresh(&conn, agg.id);
        let pick = pick_plan(&conn, &agg, "m2").unwrap().unwrap();
        assert_eq!(pick.plan.id, ids[1]);
        assert_eq!(peek_plan(&conn, &agg).unwrap(), Some(ids[1]));
    }

    #[test]
    fn model_match_multiple_least_used_wins() {
        // 当前 p1 未配置模型；p2/p3/p4 都匹配 m1，用量 100/50/50
        // -> 最少为 p3/p4 并列，取先绑定的 p3
        let (conn, agg, ids) = setup_model(&[&[], &["m1"], &["m1"], &["m1"]]);
        db_mod::set_current_plan(&conn, agg.id, ids[0]).unwrap();
        for b in db_mod::list_bindings(&conn, agg.id).unwrap() {
            let usage = if b.plan.id == ids[1] { 100 } else { 50 };
            db_mod::add_binding_usage(&conn, b.binding_id, usage).unwrap();
        }

        let agg = fresh(&conn, agg.id);
        let pick = pick_plan(&conn, &agg, "m1").unwrap().unwrap();
        assert_eq!(pick.plan.id, ids[2], "多匹配取用量最少者，同量取先绑定者");
        assert_eq!(current(&conn, agg.id), Some(ids[2]));
    }

    #[test]
    fn model_match_no_match_uses_current() {
        // 未知模型 / 未携带模型 -> 当前计划
        let (conn, agg, ids) = setup_model(&[&[], &["m2"]]);
        db_mod::set_current_plan(&conn, agg.id, ids[1]).unwrap();

        let agg = fresh(&conn, agg.id);
        assert_eq!(
            pick_plan(&conn, &agg, "unknown").unwrap().unwrap().plan.id,
            ids[1]
        );
        assert_eq!(pick_plan(&conn, &agg, "").unwrap().unwrap().plan.id, ids[1]);
        assert_eq!(
            current(&conn, agg.id),
            Some(ids[1]),
            "无匹配不应改写当前计划"
        );
    }

    #[test]
    fn model_match_invalid_current_falls_back_to_first_enabled() {
        let (conn, agg, ids) = setup_model(&[&["m1"], &[]]);

        // current 未设置 -> 回退第一个启用绑定并固化
        let pick = pick_plan(&conn, &agg, "").unwrap().unwrap();
        assert_eq!(pick.plan.id, ids[0]);
        assert_eq!(
            current(&conn, agg.id),
            Some(ids[0]),
            "回退应固化，避免每请求漂移"
        );

        // 当前计划被禁用 -> 回退到第一个启用的（p2），禁用的计划即使模型匹配也不参与
        db_mod::update_plan(
            &conn,
            ids[0],
            "p0",
            "https://a",
            "t0",
            "",
            false,
            &["m1".into()],
        )
        .unwrap();
        let agg = fresh(&conn, agg.id);
        let pick = pick_plan(&conn, &agg, "m1").unwrap().unwrap();
        assert_eq!(pick.plan.id, ids[1]);
        assert_eq!(current(&conn, agg.id), Some(ids[1]));
    }

    #[test]
    fn model_match_empty_models_never_match() {
        // 未配置模型的计划不因模型匹配被选中，仅作为当前计划承接流量
        let (conn, agg, ids) = setup_model(&[&[], &["m1"]]);
        db_mod::set_current_plan(&conn, agg.id, ids[0]).unwrap();

        let agg = fresh(&conn, agg.id);
        assert_eq!(
            pick_plan(&conn, &agg, "m1").unwrap().unwrap().plan.id,
            ids[1]
        );

        // 切换后当前为 p2，空匹配请求走当前计划
        let agg = fresh(&conn, agg.id);
        assert_eq!(
            pick_plan(&conn, &agg, "whatever").unwrap().unwrap().plan.id,
            ids[1]
        );
    }

    #[test]
    fn model_match_peek_no_side_effects_and_none() {
        // peek：current 未设置时展示回退目标但不落库
        let (conn, agg, ids) = setup_model(&[&["m1"], &[]]);
        assert_eq!(peek_plan(&conn, &agg).unwrap(), Some(ids[0]));
        assert_eq!(current(&conn, agg.id), None, "peek 不应写 current");

        db_mod::set_current_plan(&conn, agg.id, ids[1]).unwrap();
        let agg = fresh(&conn, agg.id);
        assert_eq!(peek_plan(&conn, &agg).unwrap(), Some(ids[1]));

        // 全部禁用 -> None（与 pick 的 503 语义对应）
        for id in &ids {
            db_mod::update_plan(&conn, *id, "x", "https://a", "t", "", false, &[]).unwrap();
        }
        assert_eq!(peek_plan(&conn, &agg).unwrap(), None);
        assert!(pick_plan(&conn, &agg, "m1").unwrap().is_none());
    }
}
