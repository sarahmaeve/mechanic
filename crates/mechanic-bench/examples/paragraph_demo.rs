//! A paced reading showcase. Paragraphs stay intact; the terminal wraps them.

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

struct Section {
    language: &'static str,
    title: &'static str,
    paragraphs: &'static [&'static str],
}

const SECTIONS: &[Section] = &[
    Section {
        language: "English",
        title: "Morning, slowly",
        paragraphs: &[
            "The first train had not arrived when Mara opened the garden gate. Rain still rested on the leaves, and a blackbird was practicing the same three notes from the fence. She carried a notebook rather than a camera: today she wanted to notice the small changes that a photograph might hurry past, from the new curl of a fern to the muddy footprints beside the watering can.",
            "At the station café, someone had pinned a handwritten map above the counter. It marked a quiet path along the river, a bench with a view of the old bridge, and a bakery that opened at 07:30. None of these places was especially famous. Together, however, they made a convincing invitation to spend a morning without treating every minute as a problem to solve.",
            "By noon, the notebook held five observations and one unanswered question. Mara left a generous space beneath the question, bought a warm loaf, and took the longer route home. The town had not changed very much; the pace of her attention had. A paragraph, she thought, could work like that walk: room enough for a thought to turn a corner before it reached its destination.",
        ],
    },
    Section {
        language: "Français",
        title: "Un carnet au bord de l’eau",
        paragraphs: &[
            "Le marché s’installait doucement sur la place lorsque Camille est arrivée avec son panier vide. Une vendeuse disposait des poires encore couvertes de rosée, tandis que le boulanger racontait pourquoi sa nouvelle fournée avait pris quelques minutes de retard. Personne ne semblait pressé d’effacer ce petit détour dans la conversation : il faisait déjà partie de la matinée.",
            "Au bord du canal, Camille a retrouvé le banc où elle venait lire après les cours. Le bois avait été repeint, mais la vue restait familière : deux péniches, une passerelle et le reflet tremblant des fenêtres. Elle a noté dans son carnet une phrase entendue au marché, puis une autre de son invention. Les accents, les hésitations et les silences avaient tous leur place sur la page.",
            "Vers 16 h 20, un garçon a demandé si le chemin menait jusqu’au jardin public. Camille lui a montré la passerelle, puis le sentier derrière les peupliers. Elle a refermé son carnet sans terminer la dernière phrase. Il lui plaisait qu’une journée puisse rester ouverte, comme une porte entrebâillée sur une pièce où quelqu’un prépare encore du thé.",
        ],
    },
    Section {
        language: "中文",
        title: "雨后的街道",
        paragraphs: &[
            "雨停以后，街道上的声音慢慢清晰起来。早餐店的老板把门口的椅子擦干，送报的人沿着骑楼走过，远处的电车在路口轻轻响铃。林然没有急着赶路，而是在书店门前停了一会儿。玻璃窗里摆着一本关于旧城的小册子，封面上的桥，正是她每天经过却很少仔细看过的那一座。",
            "小册子记录了桥边几家店的故事：修伞的师傅从年轻时一直做到现在，花店每周三会收到新的枝叶，面包房的招牌已经换过三次。这些事情没有宏大的结论，却让一条普通的街道显得具体而亲切。林然读到第二十页时，忽然想起小时候放学回家的路线，也想起母亲总会在拐角等她。",
            "下午三点，她带着新买的书走到河边。水面映着云，台阶上坐着几位聊天的邻居，孩子们正在商量下一场游戏。她在空白页上写下日期：2026年10月2日。然后又写了一句很简单的话：今天，我把熟悉的地方重新看了一遍。写完以后，她把书合上，留下一小段没有安排的时间。",
        ],
    },
    Section {
        language: "العربية",
        title: "المدينة بعد المطر",
        paragraphs: &[
            "بعد أن توقف المطر، خرجت سلمى إلى الشارع الذي يصل المكتبة بالساحة القديمة. كانت قطرات الماء تلمع على أوراق الأشجار، وكان صاحب المقهى يعيد ترتيب الكراسي قرب الباب. لم تكن في عجلة من أمرها؛ أرادت أن ترى كيف يعود الحي إلى إيقاعه المعتاد، وأن تسمع الأخبار الصغيرة التي يتبادلها الناس قبل أن يبدأ يومهم الطويل.",
            "عند مدخل المكتبة، قرأت إعلانًا عن مشروع جديد لحفظ ذاكرة الحي. تبدأ اللقاءات في ١٥ أكتوبر ٢٠٢٦، ويشارك فيها 24 متطوعًا لجمع الصور والخرائط والشهادات. كتب الفريق عبارة «Open Data» بجانب رابط الأرشيف، ثم أوضح بالعربية أن المواد ستكون متاحة للجميع. سألت سلمى الموظفة: هل يمكن أن نضيف حكايات لم تُكتب من قبل؟ فأجابتها بابتسامة: لهذا بدأنا المشروع.",
            "في طريق العودة، توقفت سلمى عند الجسر الصغير واستعادت قصة كانت جدتها ترويها عن السوق. دوّنت الأسماء والتواريخ، وتركت مساحة لما لم تتذكره بعد. لم تكن تريد أن تبدو الحكاية مكتملة قبل أوانها؛ فبعض التفاصيل لا تعود إلا حين نصغي إلى شخص آخر. وحين دقت الساعة 17:30، أغلقت دفترها ومضت إلى البيت، وهي تفكر في السؤال الذي ستطرحه في اللقاء الأول.",
        ],
    },
    Section {
        language: "日本語",
        title: "夕方の小さな図書館",
        paragraphs: &[
            "夕方の図書館には、ページをめくる音と、窓の外を通る自転車の音が静かに重なっていた。美咲は返却する本をカウンターに置き、入口に貼られた手書きの案内を読んだ。来週は、町の古い地図を囲んで思い出を話す会があるという。いつも通り過ぎていた通りにも、まだ知らない名前や物語が残っているのかもしれない。",
            "地図の棚で見つけた一枚には、今は公園になっている場所に小さな駅が描かれていた。美咲が眺めていると、隣の席の人が、昔はそこで祖父を待っていたと教えてくれた。話は列車の時刻から、駅前のパン屋、夏祭りの灯りへとゆっくり移っていった。紙の上の線が、誰かの記憶に触れることで、歩ける道に変わっていくようだった。",
            "時計が18時を指す少し前、美咲はノートに今日の話を三つ書き留めた。最後のページには、次に聞きたいことを一つだけ残した。図書館を出ると、空にはまだ明るさがあり、川沿いの道から夕食の匂いが届いた。急いで結論をつけなくてもいい。そう思いながら、彼女はいつもより少し遠回りをして家に帰った。",
        ],
    },
];

fn plain() -> io::Result<()> {
    let mut out = io::stdout().lock();
    for section in SECTIONS {
        writeln!(out, "{} · {}", section.language, section.title)?;
        for paragraph in section.paragraphs {
            writeln!(out, "{paragraph}")?;
        }
        writeln!(out)?;
    }
    Ok(())
}

#[cfg(unix)]
mod interactive {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    struct Terminal {
        fd: libc::c_int,
        saved: libc::termios,
        cancelled: Arc<AtomicBool>,
        signals: Vec<signal_hook::SigId>,
    }

    impl Terminal {
        fn enter(fd: libc::c_int) -> io::Result<Self> {
            let mut saved = std::mem::MaybeUninit::uninit();
            // SAFETY: saved is writable termios storage; tcgetattr validates fd.
            if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut terminal = Self {
                fd,
                // SAFETY: tcgetattr initialized saved successfully.
                saved: unsafe { saved.assume_init() },
                cancelled: Arc::new(AtomicBool::new(false)),
                signals: Vec::new(),
            };
            for signal in [
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
                signal_hook::consts::SIGHUP,
                signal_hook::consts::SIGQUIT,
                signal_hook::consts::SIGTSTP,
            ] {
                terminal
                    .signals
                    .push(signal_hook::flag::register(signal, Arc::clone(&terminal.cancelled))?);
            }
            let mut raw = terminal.saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_iflag &= !(libc::ICRNL | libc::IXON);
            raw.c_oflag &= !libc::OPOST;
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            // SAFETY: raw is initialized and the terminal descriptor remains open.
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(terminal)
        }
    }

    impl Drop for Terminal {
        fn drop(&mut self) {
            // SAFETY: saved came from this live terminal descriptor.
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
            }
            for signal in self.signals.drain(..) {
                signal_hook::low_level::unregister(signal);
            }
            let _ = io::stdout().write_all(b"\x1b[0m\r\n");
        }
    }

    fn key(timeout: Option<Duration>) -> io::Result<Option<u8>> {
        let mut input: libc::fd_set = unsafe { std::mem::zeroed() };
        let mut delay = timeout.map(|duration| libc::timeval {
            tv_sec: duration.as_secs() as _,
            tv_usec: duration.subsec_micros() as _,
        });
        // SAFETY: stdin fits fd_set and both pointers remain valid through select.
        let ready = unsafe {
            libc::FD_ZERO(&mut input);
            libc::FD_SET(libc::STDIN_FILENO, &mut input);
            libc::select(
                1,
                &mut input,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                delay.as_mut().map_or(std::ptr::null_mut(), |value| value as *mut _),
            )
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted { Ok(None) } else { Err(error) };
        }
        if ready == 0 {
            return Ok(None);
        }
        let mut byte = 0u8;
        // SAFETY: byte is writable for one byte and stdin is open.
        let count = unsafe { libc::read(libc::STDIN_FILENO, (&mut byte as *mut u8).cast(), 1) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(Some(byte))
    }

    pub fn run() -> io::Result<()> {
        let terminal = Terminal::enter(libc::STDIN_FILENO)?;
        let mut out = io::stdout();
        write!(
            out,
            "\r\n\x1b[1mA little room to read\x1b[0m\r\n\x1b[2mFive languages · automatic wrapping · ordinary scrollback\r\nSpace pause/resume · R replay · Q or Esc quit\x1b[0m\r\n"
        )?;
        out.flush()?;
        let mut section = 0;
        let mut paragraph = 0;
        let mut paused = false;
        let mut next = Instant::now() + Duration::from_millis(600);
        while !terminal.cancelled.load(Ordering::Relaxed) {
            if !paused && section < SECTIONS.len() && Instant::now() >= next {
                let current = &SECTIONS[section];
                if paragraph == 0 {
                    write!(
                        out,
                        "\r\n\x1b[38;2;112;196;214m{:02} / {:02}  {} · {}\x1b[0m\r\n\r\n",
                        section + 1,
                        SECTIONS.len(),
                        current.language,
                        current.title
                    )?;
                }
                // One unbroken logical line: no column counting or manual wraps.
                out.write_all(current.paragraphs[paragraph].as_bytes())?;
                out.write_all(b"\r\n\r\n")?;
                paragraph += 1;
                if paragraph == current.paragraphs.len() {
                    section += 1;
                    paragraph = 0;
                }
                if section == SECTIONS.len() {
                    write!(
                        out,
                        "\x1b[2mEnd of the walk. Scroll back to explore; R replays, Q quits.\x1b[0m\r\n"
                    )?;
                }
                out.flush()?;
                next = Instant::now() + Duration::from_secs(4);
            }
            let timeout = if paused || section == SECTIONS.len() {
                Duration::from_millis(250)
            } else {
                next.saturating_duration_since(Instant::now()).min(Duration::from_millis(250))
            };
            // Bounded waits also catch a signal delivered just before select starts.
            match key(Some(timeout)) {
                Ok(Some(b'q' | b'Q' | 0x1b | 0x03)) => break,
                Ok(Some(b' ')) if section < SECTIONS.len() => {
                    paused = !paused;
                    write!(
                        out,
                        "\x1b[2m{}\x1b[0m\r\n",
                        if paused { "Paused · Space continues" } else { "Continuing…" }
                    )?;
                    out.flush()?;
                    if !paused {
                        next = Instant::now() + Duration::from_millis(600);
                    }
                }
                Ok(Some(b'r' | b'R')) => {
                    write!(out, "\r\n\x1b[2m──────── A fresh reading ────────\x1b[0m\r\n")?;
                    out.flush()?;
                    section = 0;
                    paragraph = 0;
                    paused = false;
                    next = Instant::now() + Duration::from_millis(600);
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        fn attributes(fd: libc::c_int) -> libc::termios {
            let mut value = std::mem::MaybeUninit::uninit();
            // SAFETY: value is writable termios storage and fd is an open PTY.
            assert_eq!(unsafe { libc::tcgetattr(fd, value.as_mut_ptr()) }, 0);
            // SAFETY: tcgetattr initialized value successfully.
            unsafe { value.assume_init() }
        }

        #[test]
        fn terminal_guard_restores_pty_settings() {
            let mut master = -1;
            let mut slave = -1;
            // SAFETY: openpty writes two descriptors; optional arguments are null.
            let result = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(result, 0, "openpty failed: {}", io::Error::last_os_error());
            // SAFETY: openpty returned owned descriptors, each wrapped exactly once.
            let (_master, slave) =
                unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
            let before = attributes(slave.as_raw_fd());
            assert_ne!(before.c_lflag & libc::ICANON, 0);
            assert_ne!(before.c_lflag & libc::ECHO, 0);
            let terminal = Terminal::enter(slave.as_raw_fd()).unwrap();
            let active = attributes(slave.as_raw_fd());
            assert_eq!(active.c_lflag & (libc::ICANON | libc::ECHO), 0);
            drop(terminal);
            let after = attributes(slave.as_raw_fd());
            assert_eq!(after.c_iflag, before.c_iflag);
            assert_eq!(after.c_oflag, before.c_oflag);
            assert_eq!(after.c_cflag, before.c_cflag);
            // The kernel may set PENDIN when switching back to canonical mode.
            assert_eq!(after.c_lflag & !libc::PENDIN, before.c_lflag & !libc::PENDIN);
            assert_eq!(after.c_cc, before.c_cc);
            // SAFETY: both termios values are initialized and remain live.
            unsafe {
                assert_eq!(libc::cfgetispeed(&after), libc::cfgetispeed(&before));
                assert_eq!(libc::cfgetospeed(&after), libc::cfgetospeed(&before));
            }
        }
    }
}

fn run() -> io::Result<()> {
    let mut plain_mode = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--plain" => plain_mode = true,
            "--help" | "-h" => {
                println!(
                    "paragraph_demo [--plain]\n\nA paced multilingual reading showcase with native wrapping and scrollback.\nSpace pauses/resumes; R replays; Q or Esc quits. Ctrl-C also restores the terminal.\n--plain prints clean logical paragraphs immediately, without terminal controls."
                );
                return Ok(());
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown option {arg:?}; use --help"),
                ));
            }
        }
    }
    if plain_mode || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return plain();
    }
    #[cfg(unix)]
    {
        interactive::run()
    }
    #[cfg(not(unix))]
    {
        plain()
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("paragraph demo: {error}");
            ExitCode::FAILURE
        }
    }
}
