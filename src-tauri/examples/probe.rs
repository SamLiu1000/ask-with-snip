//! 截屏探针：脱离 GUI 验证"抓主屏 -> 裁剪 -> 压缩编码"这条链路。
//!
//! 用法（在 src-tauri 目录下）：
//!     cargo run --example probe
//!     cargo run --example probe -- 100 100 800 600     # 额外裁一块并检查尺寸
//!
//! 排查"框选出来全是黑的""尺寸不对"这类问题时，先跑它，
//! 能立刻分清是截屏本身有问题，还是坐标换算/窗口的问题。
//!
//! 产物写到系统临时目录下的 ask-with-snip-probe 文件夹，可以直接打开看。

use ask_with_snip_lib::capture;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    println!("== 抓取主显示器 ==");
    let frame = match capture::capture_primary() {
        Ok(frame) => frame,
        Err(err) => {
            eprintln!("截屏失败: {err}");
            eprintln!("如果报权限或空图像，检查一下当前会话是否有桌面访问权限。");
            std::process::exit(1);
        }
    };

    println!("分辨率(物理像素): {} x {}", frame.width(), frame.height());
    println!("缩放比例: {}", frame.scale_factor);
    println!("显示器原点: ({}, {})", frame.origin_x, frame.origin_y);

    // 全黑/全白检测：截屏 API 在权限不足时常常返回一张纯色图而不是报错
    let sample = capture::crop(&frame, 0, 0, frame.width(), frame.height())
        .expect("整屏裁剪不该失败");
    let mut min = 255u8;
    let mut max = 0u8;
    for pixel in sample.pixels().step_by(97) {
        let luma = ((pixel[0] as u32 * 30 + pixel[1] as u32 * 59 + pixel[2] as u32 * 11) / 100) as u8;
        min = min.min(luma);
        max = max.max(luma);
    }
    println!("亮度范围: {min} ~ {max}");
    if max - min < 3 {
        println!("警告: 画面几乎是纯色，很可能截屏权限被拒（或者屏幕真的全黑）");
    } else {
        println!("画面有内容，截屏正常");
    }

    let out_dir = std::env::temp_dir().join("ask-with-snip-probe");
    std::fs::create_dir_all(&out_dir).expect("无法创建输出目录");

    let full = capture::to_data_url_jpeg(&frame.image, 0, 92).expect("整屏编码失败");
    report("整屏背景图", &full);

    // 可选：按给定矩形裁一块，验证裁剪和压缩
    if args.len() == 4 {
        let nums: Vec<u32> = args.iter().filter_map(|a| a.parse().ok()).collect();
        if nums.len() == 4 {
            let (x, y, w, h) = (nums[0], nums[1], nums[2], nums[3]);
            println!("\n== 裁剪 ({x}, {y}) {w} x {h} ==");
            match capture::crop(&frame, x as i32, y as i32, w, h) {
                Ok(cropped) => {
                    println!("实际裁剪尺寸: {} x {}", cropped.width(), cropped.height());
                    let encoded =
                        capture::encode_jpeg_base64(&cropped, 1568, 85).expect("裁剪编码失败");
                    report("裁剪结果", &encoded);
                }
                Err(err) => eprintln!("裁剪失败: {err}"),
            }
        }
    }

    // 把整屏存成真正的文件，方便肉眼确认内容对不对
    let path = out_dir.join("full.jpg");
    let encoded = capture::encode_jpeg_base64(&frame.image, 1920, 85).expect("落盘编码失败");
    match base64_decode_to_file(&encoded, &path) {
        Ok(()) => println!("\n已写出样张: {}", path.display()),
        Err(err) => println!("\n样张写出失败: {err}"),
    }
}

/// 打印一张 data url 的体积与像素尺寸（数据本身不打印，几 MB 太长）
fn report(label: &str, data_url: &str) {
    let b64 = data_url.split(',').nth(1).unwrap_or(data_url);
    let bytes = b64.len() * 3 / 4;
    println!("{label}: base64 约 {} KB", bytes / 1024);
}

fn base64_decode_to_file(b64: &str, path: &std::path::Path) -> Result<(), String> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| e.to_string())?;
    std::fs::write(path, raw).map_err(|e| e.to_string())
}
