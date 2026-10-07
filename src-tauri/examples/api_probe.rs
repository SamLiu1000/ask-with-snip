//! 接口链路探针：对着一个真实的 HTTP 服务验证"组装请求 -> 流式收包 -> 解析 SSE"。
//!
//! 配合 tools/mock_openai_server.py 使用，不需要真实 API Key、不产生费用：
//!
//!     # 终端 1
//!     python tools/mock_openai_server.py 8899
//!     # 终端 2
//!     cd src-tauri && cargo run --example api_probe
//!
//! 它会验证四件事：
//!   1. 非流式请求能拿到回复（设置页"测试连接"走的就是这条）
//!   2. 流式请求能逐段收到增量，且拼起来等于服务端的完整回答
//!   3. 第一轮带图片、追问不带图片（省 token 的关键行为）
//!   4. 连不上的时候返回的是可读错误，而不是 panic

use base64::Engine as _;
use ask_with_snip_lib::api;
use ask_with_snip_lib::config::Profile;
use ask_with_snip_lib::history::Turn;
use serde_json::json;

const BASE: &str = "http://127.0.0.1:8899/v1";
const TIMEOUT: u64 = 20;
/// 假接口把请求摘要写在这里。探针的工作目录是 src-tauri，所以往上走一层。
const LOG_PATH: &str = "../tools/mock_requests.jsonl";

fn probe_profile() -> Profile {
    Profile {
        name: "探针".to_string(),
        base_url: BASE.to_string(),
        api_key: "test-key".to_string(),
        model: "mock-vl".to_string(),
        extra_headers: String::new(),
        proxy: String::new(),
    }
}

/// 造一张真实的 JPEG 并转成 data url，模拟截图发出去的样子
fn sample_image_data_url() -> String {
    let img = image::RgbImage::from_fn(320, 200, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    });
    let mut buf = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85);
    encoder.encode_image(&img).expect("样例图编码失败");
    format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&buf)
    )
}

fn tail_log(count: usize) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(LOG_PATH) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect::<Vec<serde_json::Value>>()
        .into_iter()
        .rev()
        .take(count)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[tokio::main]
async fn main() {
    let profile = probe_profile();
    println!("目标接口: {}", profile.endpoint());
    println!("（先用 python tools/mock_openai_server.py 8899 起一个假接口）\n");

    // ---- 1. 非流式 ----
    println!("== 1. 非流式请求（测试连接）==");
    match api::test_connection(&profile, TIMEOUT).await {
        Ok(reply) => println!("模型回复: {reply}"),
        Err(err) => {
            eprintln!("失败: {err}");
            eprintln!("\n如果提示连接被拒绝，说明假接口没起起来。");
            std::process::exit(1);
        }
    }

    // ---- 2. 流式：第一轮带图 ----
    println!("\n== 2. 流式请求（第一轮，带图）==");
    let image = sample_image_data_url();
    println!("图片 data url 长度: {} 字符", image.len());

    // 图片在历史里是裸 base64（不带 data: 前缀），messages_from_turns 负责加前缀
    let image_b64 = image
        .strip_prefix("data:image/jpeg;base64,")
        .unwrap_or(&image)
        .to_string();

    let turns = vec![Turn {
        question: "这张图里有什么？".to_string(),
        answer: String::new(),
        images: vec![image_b64.clone()],
        ..Default::default()
    }];
    let history = api::messages_from_turns("", &turns);
    let mut deltas: Vec<String> = Vec::new();
    let answer = match api::stream_chat_with(
        &profile,
        TIMEOUT,
        history,
        &api::Cancel::new(),
        |chunk| match chunk {
            api::Chunk::Text(delta) => {
                println!("  <- {:?}", delta);
                deltas.push(delta);
            }
            // 思维链单独一路（推理模型才有）
            api::Chunk::Reasoning(delta) => println!("  << 思考 {:?}", delta),
        },
    )
    .await
    {
        Ok(answer) => answer,
        Err(err) => {
            eprintln!("失败: {err}");
            std::process::exit(1);
        }
    };
    println!("完整回答 {} 字", answer.chars().count());

    let joined = deltas.concat();
    if joined == answer {
        println!("增量拼接 == 完整回答，一致");
    } else {
        eprintln!("警告: 增量拼接和完整回答不一致");
    }
    if answer.contains("hello from mock") {
        println!("回答内容符合预期（假接口的固定文本）");
    }

    // ---- 3. 追问不带图，再截一张又带新图 ----
    println!("\n== 3. 流式请求（追问 + 再接一张新截图）==");
    let follow_up_turns = vec![
        Turn {
            question: "这张图里有什么？".to_string(),
            answer: answer.clone(),
            images: vec![image_b64.clone()],
            ..Default::default()
        },
        // 追问：没有新截图，这一轮不该带图
        Turn {
            question: "再详细一点".to_string(),
            answer: "好".to_string(),
            images: Vec::new(),
            ..Default::default()
        },
        // "接着上文继续框选"新截的一张：这一轮要带它自己的新图
        Turn {
            question: "再看这一块".to_string(),
            answer: String::new(),
            images: vec![image_b64.clone()],
            ..Default::default()
        },
    ];
    let follow_up = api::messages_from_turns("", &follow_up_turns);
    let with_image = follow_up.iter().filter(|m| m["content"].is_array()).count();
    println!("消息数 {}（带图的消息 {} 条，应是 2 条）", follow_up.len(), with_image);
    match api::stream_chat_with(&profile, TIMEOUT, follow_up, &api::Cancel::new(), |_| {}).await {
        Ok(text) => println!("第二次回答 {} 字", text.chars().count()),
        Err(err) => eprintln!("第二次提问失败: {err}"),
    }

    // ---- 4. 核对服务端实际收到的内容 ----
    println!("\n== 4. 核对服务端收到的请求 ==");
    let entries = tail_log(3);
    if entries.is_empty() {
        println!("没有读到日志（{LOG_PATH}），跳过核对");
    } else {
        for entry in &entries {
            println!(
                "  {} stream={} messages={} images={} image_chars={}",
                entry["time"], entry["stream"], entry["message_count"],
                entry["image_count"], entry["image_base64_chars"]
            );
        }

        let streamed: Vec<&serde_json::Value> = entries
            .iter()
            .filter(|e| e["stream"] == json!(true))
            .collect();

        if streamed.len() >= 2 {
            let first = streamed[streamed.len() - 2];
            let second = streamed[streamed.len() - 1];

            // 关键行为一：第一轮带 1 张图
            if first["image_count"] == json!(1) {
                println!("第一轮带了 1 张图，符合预期");
            } else {
                eprintln!("警告: 第一轮应该带 1 张图，实际 {}", first["image_count"]);
            }
            // 关键行为二：追问不带新图，但新截的那一轮要带它自己的图 —— 所以是三张图里带两张：
            // 第一轮那张 + 第三轮新截的那张，中间那轮（纯追问）不带。
            if second["image_count"] == json!(2) {
                println!("第二次请求带了 2 张图（第一轮 + 新截的那张），符合预期");
            } else {
                eprintln!(
                    "警告: 第二次请求应该带 2 张图（原图 + 新截图），实际 {}",
                    second["image_count"]
                );
            }
            if second["message_count"].as_u64() > first["message_count"].as_u64() {
                println!("第二次请求带上了更长的上下文，符合预期");
            }
        }
    }

    // ---- 5. 连不上的时候要给出可读错误 ----
    println!("\n== 5. 错误路径（指向一个没人监听的端口）==");
    let broken = Profile {
        base_url: "http://127.0.0.1:9/v1".to_string(),
        ..profile.clone()
    };
    match api::test_connection(&broken, TIMEOUT).await {
        Ok(text) => eprintln!("警告: 居然成功了？{text}"),
        Err(err) => println!("按预期失败，错误信息: {err}"),
    }

    println!("\n接口链路验证结束。");
}
