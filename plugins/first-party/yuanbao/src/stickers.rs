//! The built-in sticker pack (package 1003, 128x128 PNG): name, id and
//! the words people use for it. Search scores a query against names
//! first, then descriptions.

/// (name, sticker id, description).
pub(crate) const STICKERS: &[(&str, &str, &str)] = &[
    ("六六六", "278", "666 厉害 牛 棒 绝了 好强 awesome"),
    ("我想开了", "262", "想开 佛系 释怀 顿悟 看淡了 无所谓"),
    ("害羞", "130", "腼腆 不好意思 脸红 娇羞 羞涩 捂脸"),
    ("比心", "252", "笔芯 爱你 爱心手势 love heart 喜欢你"),
    ("委屈", "125", "难过 想哭 可怜巴巴 瘪嘴 受伤 被欺负"),
    ("亲亲", "146", "么么 mua 亲一下 kiss 飞吻 啵"),
    ("酷", "131", "帅 墨镜 cool 高冷 有型 swagger"),
    ("睡", "145", "睡觉 困 zzZ 打盹 躺平 休眠 sleepy"),
    ("发呆", "152", "懵 愣住 放空 呆滞 出神 脑子空白"),
    ("可怜", "157", "卖萌 求饶 委屈巴巴 弱小 拜托 眼巴巴"),
    ("摊手", "200", "无奈 没办法 耸肩 随便 那咋整 whatever"),
    ("头大", "213", "头疼 烦恼 郁闷 难搞 崩溃 一团乱"),
    ("吓", "256", "害怕 惊恐 震惊 吓一跳 恐怖 怂"),
    ("吐血", "203", "无语 崩溃 被雷 内伤 一口老血 屮"),
    ("哼", "185", "傲娇 生气 不满 撇嘴 不理 赌气"),
    ("嘿嘿", "220", "坏笑 猥琐笑 偷笑 憨笑 得意 你懂的"),
    ("头秃", "218", "程序员 加班 焦虑 没头发 秃了 肝爆"),
    ("暗中观察", "221", "窥屏 潜水 偷偷看 角落 围观 屏住呼吸"),
    ("我酸了", "224", "嫉妒 柠檬精 羡慕 吃柠檬 眼红 恰柠檬"),
    ("打call", "246", "应援 加油 支持 喝彩 助威 call"),
    ("庆祝", "251", "祝贺 开心 耶 party 胜利 干杯"),
    ("奋斗", "151", "努力 加油 拼搏 冲 干劲 卷起来"),
    ("惊讶", "143", "震惊 哇 不敢相信 OMG 居然 这么离谱"),
    ("疑问", "144", "问号 不懂 啥 为什么 啥情况 懵逼问"),
    ("仔细分析", "248", "思考 推敲 认真 研究 琢磨 让我想想"),
    ("撅嘴", "184", "嘟嘴 卖萌 不高兴 撒娇 嘴翘"),
    ("泪奔", "199", "大哭 伤心 破防 感动哭 泪流满面 呜呜"),
    ("尊嘟假嘟", "276", "真的假的 真假 可爱问 你骗我 是不是"),
    ("略略略", "113", "调皮 吐舌 不服 略 气死你 鬼脸"),
    ("困", "180", "想睡 倦 打哈欠 睁不开眼 好困啊 sleepy"),
    ("折磨", "181", "难受 痛苦 煎熬 蚌埠住了 受不了 要命"),
    ("抠鼻", "182", "不屑 无聊 淡定 无所谓 鄙视 挖鼻"),
    ("鼓掌", "183", "拍手 叫好 赞同 666 喝彩 掌声"),
    ("斜眼笑", "204", "滑稽 坏笑 doge 意味深长 阴阳怪气 嘿嘿嘿"),
    ("辣眼睛", "216", "看不下去 cringe 毁三观 太丑了 瞎了"),
    ("哦哟", "217", "惊讶 起哄 哇哦 有戏 不简单 哟"),
    ("吃瓜", "222", "围观 看戏 八卦 路人 看热闹 板凳"),
    ("狗头", "225", "doge 保命 开玩笑 滑稽 反讽 懂的都懂"),
    ("敬礼", "227", "salute 尊重 收到 遵命 致敬 报告"),
    ("哦", "231", "知道了 明白 敷衍 嗯 这样啊 收到"),
    ("拿到红包", "236", "红包 谢谢老板 发财 开心 抢到了 欧气"),
    ("牛吖", "239", "牛 厉害 强 666 佩服 大佬"),
    ("贴贴", "272", "抱抱 亲昵 蹭蹭 亲密 靠靠 撒娇贴"),
    ("爱心", "138", "心 love 喜欢你 红心 示爱 么么哒"),
    ("晚安", "170", "好梦 睡了 night 早点休息 安啦 moon"),
    ("太阳", "176", "晴天 早上好 阳光 morning 好天气 日"),
    ("柠檬", "266", "酸 嫉妒 柠檬精 羡慕 我酸 恰柠檬"),
    ("大冤种", "267", "倒霉 吃亏 自嘲 好心没好报 背锅 工具人"),
    ("吐了", "132", "恶心 yue 受不了 嫌弃 想吐 生理不适"),
    ("怒", "134", "生气 愤怒 火大 暴躁 气炸 怼"),
    ("玫瑰", "165", "花 示爱 表白 浪漫 送你花 情人节"),
    ("凋谢", "119", "花谢 失恋 难过 枯萎 心碎 凉了"),
    ("点赞", "159", "赞 认同 好棒 good like 大拇指 顶"),
    ("握手", "164", "合作 你好 商务 hello deal 成交 友好"),
    ("抱拳", "163", "谢谢 失敬 江湖 承让 拜托 有礼"),
    ("ok", "169", "好的 收到 没问题 okay 行 可以 懂了"),
    ("拳头", "174", "加油 干 冲 fight 力量 击拳 硬气"),
    ("鞭炮", "191", "过年 喜庆 爆竹 春节 噼里啪啦 红"),
    ("烟花", "258", "庆典 漂亮 新年 嘭 绽放 节日快乐"),
];

/// The most the search tool returns.
pub(crate) const MAX_RESULTS: usize = 50;

fn score(query: &str, name: &str, description: &str) -> u32 {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return 1;
    }
    let name = name.to_lowercase();
    let description = description.to_lowercase();
    if name == q {
        return 100;
    }
    if name.contains(&q) || q.contains(&name) {
        return 80;
    }
    if description.split_whitespace().any(|w| w == q) {
        return 60;
    }
    if description.contains(&q) {
        return 40;
    }
    // Characters of the query found in the name or description.
    let hits = q
        .chars()
        .filter(|c| !c.is_whitespace() && (name.contains(*c) || description.contains(*c)))
        .count();
    let total = q.chars().filter(|c| !c.is_whitespace()).count().max(1);
    u32::try_from(hits * 30 / total).unwrap_or(0)
}

/// The stickers best matching `query`, at most `limit`.
pub(crate) fn search(query: &str, limit: usize) -> Vec<(&'static str, &'static str, &'static str)> {
    let mut scored: Vec<(u32, usize)> = STICKERS
        .iter()
        .enumerate()
        .map(|(i, (name, _, description))| (score(query, name, description), i))
        .filter(|(s, _)| *s >= 10)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(limit.min(MAX_RESULTS))
        .map(|(_, i)| STICKERS[i])
        .collect()
}

/// A sticker by exact name or id, else the best match.
pub(crate) fn find(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    STICKERS
        .iter()
        .find(|(n, id, _)| *n == name || *id == name)
        .copied()
        .or_else(|| search(name, 1).into_iter().next())
}

/// The `TIMFaceElem` data of a sticker.
pub(crate) fn face_data(sticker: (&str, &str, &str)) -> String {
    serde_json::json!({
        "sticker_id": sticker.1, "package_id": "1003", "width": 128, "height": 128,
        "formats": "png", "name": sticker.0,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_ranks_names_first_and_is_bounded() {
        assert_eq!(search("点赞", 10)[0].0, "点赞");
        assert!(search("", 500).len() <= MAX_RESULTS);
        assert_eq!(find("159").unwrap().0, "点赞");
    }
}
