//! テンプレートの構造(§16.8 の単位 S2): `structure.json` の読み書きと、未注釈のテンプレートを裏で注釈する流れ。
//!
//! 注釈は、辞書を読み込んだとき(起動・外での書き換え)と画面で保存したときに、構造の無い(源泉のハッシュに
//! 構造が無い)テンプレートだけを、別スレッドで 1 件ずつ行う。画面は固めない。接続が無い・失敗したら未注釈の
//! まま(補完は今までどおり全文経路で働く。S2 では補完の振る舞いは変えない)。失敗したテンプレートは同じ起動の
//! 中では裏で付け直さない(辞書の窓の「付け直す」は押せば行う)。`dictionary.json` は書かない。

use super::*;
use crate::core::annotate::{self, review, ANNOTATION_MODEL};
use crate::core::dictionary::Template;
use crate::core::structure::{source_hash, StructureFile, TemplateStructure, Vocabulary};
use std::collections::{BTreeMap, BTreeSet};

/// 注釈の口(テンプレート・語彙 → 構造)。試験では偽物に差し替え、Claude を呼ばない。
pub type AnnotateFn =
    Arc<dyn Fn(&Template, &Vocabulary) -> Result<TemplateStructure, String> + Send + Sync>;

/// 裏の注釈の答え。
struct AnnotationDone {
    hash: String,
    /// 「付け直す」で始めた物(失敗を知らせる)。
    forced: bool,
    result: Result<TemplateStructure, String>,
}

/// テンプレートの構造と、裏の注釈の状態。
pub struct Annotations {
    /// 読み込み時と保存時に裏で注釈するか。画面の起動(`VoiceApp::new`)で撮影でなければ true。
    /// 試験と撮影は false(Claude を呼ばない)。
    pub automatic: bool,
    /// `structure.json` の中身。
    pub structures: StructureFile,
    /// 注釈中のテンプレート(源泉のハッシュ)。
    pub running: BTreeSet<String>,
    /// 注釈できなかったテンプレート(ハッシュ → 理由)。同じ起動の中では裏で付け直さない。
    pub failed: BTreeMap<String, String>,
    hook: Option<AnnotateFn>,
    tx: Sender<AnnotationDone>,
    rx: Receiver<AnnotationDone>,
}

impl Annotations {
    pub(super) fn new(structures: StructureFile) -> Self {
        let (tx, rx) = mpsc::channel();
        Annotations {
            automatic: false,
            structures,
            running: BTreeSet::new(),
            failed: BTreeMap::new(),
            hook: None,
            tx,
            rx,
        }
    }
}

/// 辞書の窓に出すテンプレートの構造の状態。
pub enum StructureState<'a> {
    Annotated(&'a TemplateStructure),
    Running,
    Failed(&'a str),
    Unannotated,
}

impl VoiceApp {
    /// 注釈の口を差し替える(試験は偽物を渡す)。
    pub fn set_annotate_hook(&mut self, hook: AnnotateFn) {
        self.annotations.hook = Some(hook);
    }

    /// 語彙(同梱 + `structure.json` の利用者の追加)。
    pub fn structure_vocabulary(&self) -> Vocabulary {
        Vocabulary::builtin().with_extras(&self.annotations.structures)
    }

    /// テンプレートの構造の状態。
    pub fn structure_state(&self, template: &Template) -> StructureState<'_> {
        let hash = source_hash(template);
        if self.annotations.running.contains(&hash) {
            return StructureState::Running;
        }
        if let Some(structure) = self.annotations.structures.templates.get(&hash) {
            return StructureState::Annotated(structure);
        }
        match self.annotations.failed.get(&hash) {
            Some(reason) => StructureState::Failed(reason),
            None => StructureState::Unannotated,
        }
    }

    /// 注釈の口。差し替えがあればそれ、無ければ設定の接続(API キーか Claude Code)で Sonnet。接続が無ければ None。
    fn annotate_fn(&self) -> Option<AnnotateFn> {
        if let Some(hook) = &self.annotations.hook {
            return Some(hook.clone());
        }
        let annotator = make_annotator(&self.store, &self.global, ANNOTATION_MODEL)?;
        Some(Arc::new(move |template: &Template, vocabulary: &Vocabulary| {
            annotate::annotate(annotator.as_ref(), template, vocabulary)
        }))
    }

    /// 構造の無いテンプレートを裏で注釈する(`automatic` のときだけ)。注釈中・失敗済みの物は飛ばす。
    pub fn queue_annotations(&mut self) {
        if !self.annotations.automatic {
            return;
        }
        let mut jobs: Vec<(String, Template)> = Vec::new();
        for template in &self.data.dictionary.templates {
            let hash = source_hash(template);
            if template.body.trim().is_empty()
                || self.annotations.structures.templates.contains_key(&hash)
                || self.annotations.running.contains(&hash)
                || self.annotations.failed.contains_key(&hash)
                || jobs.iter().any(|(h, _)| *h == hash)
            {
                continue;
            }
            jobs.push((hash, template.clone()));
        }
        if jobs.is_empty() {
            return;
        }
        let Some(annotate) = self.annotate_fn() else {
            diag::log(format!("構造: 未注釈 {} 件(Claude に接続していないので注釈しない)", jobs.len()));
            return;
        };
        diag::log(format!("構造: 未注釈 {} 件を裏で注釈する", jobs.len()));
        self.spawn_annotation(jobs, annotate, false);
    }

    /// 「付け直す」: そのテンプレートを(構造があっても)注釈し直す。注釈中なら何もしない。
    pub fn reannotate(&mut self, index: usize) {
        let Some(template) = self.data.dictionary.templates.get(index).cloned() else {
            return;
        };
        let hash = source_hash(&template);
        if self.annotations.running.contains(&hash) {
            return;
        }
        let Some(annotate) = self.annotate_fn() else {
            self.notify_error("Claude に接続していないので付け直せません。設定の「AI 訂正」で接続してください");
            return;
        };
        self.annotations.failed.remove(&hash);
        diag::log(format!("構造: 「{}」を付け直す", template.keywords.first().map_or("", String::as_str)));
        self.spawn_annotation(vec![(hash, template)], annotate, true);
    }

    fn spawn_annotation(&mut self, jobs: Vec<(String, Template)>, annotate: AnnotateFn, forced: bool) {
        for (hash, _) in &jobs {
            self.annotations.running.insert(hash.clone());
        }
        let vocabulary = self.structure_vocabulary();
        let tx = self.annotations.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            for (hash, template) in jobs {
                let result = annotate(&template, &vocabulary);
                if tx.send(AnnotationDone { hash, forced, result }).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
    }

    /// 裏の注釈の答えを受け取り、源泉がまだあれば構造に入れて保存する。失敗は記録だけ(付け直しは知らせる)。
    pub fn receive_annotations(&mut self) {
        let mut changed = false;
        while let Ok(done) = self.annotations.rx.try_recv() {
            self.annotations.running.remove(&done.hash);
            let template = self
                .data
                .dictionary
                .templates
                .iter()
                .find(|t| source_hash(t) == done.hash)
                .cloned();
            match (done.result, template) {
                (Ok(structure), Some(template)) => {
                    diag::log(format!(
                        "構造: 「{}」を注釈した(変種 {} 個、警告 {} 件)",
                        structure.name,
                        structure.variants.len(),
                        structure.unresolved.len()
                    ));
                    self.annotations.structures.insert(&template, structure);
                    changed = true;
                }
                (Ok(_), None) => diag::log("構造: 注釈の間に源泉が変わったので答えを捨てた"),
                (Err(reason), _) => {
                    diag::log(format!("構造: 注釈できない({reason})"));
                    if done.forced {
                        self.notify_error(format!("構造を付け直せませんでした: {reason}"));
                    }
                    self.annotations.failed.insert(done.hash, reason);
                }
            }
        }
        if changed {
            self.save_structures();
        }
    }

    /// 合うテンプレートの無い構造を消して `structure.json` に書く。
    pub fn save_structures(&mut self) {
        self.annotations.structures.prune(&self.data.dictionary.templates);
        if let Err(message) = self.store.save_structure(&self.annotations.structures) {
            self.notify_error(format!("構造を保存できません: {message}"));
        }
        // 補完の指示文と描画は構造を使う(単位 S3)。
        self.refresh_assistant_structure();
    }

    /// 確認表で直した構造を入れる。警告(`unresolved`)は直した構造から作り直す(注と本文のずれ・構造の検査)。
    pub fn update_structure(&mut self, index: usize, mut structure: TemplateStructure) {
        let Some(template) = self.data.dictionary.templates.get(index).cloned() else {
            return;
        };
        let vocabulary = self.structure_vocabulary();
        // 列挙の鍵を選んだ行は列挙否定に(注釈のときと同じ規則)。警告は直した構造から作り直す。
        annotate::derive_list_lines(&template, &mut structure, &vocabulary);
        structure.unresolved = review(&template, &structure, &vocabulary);
        self.annotations.structures.insert(&template, structure);
        self.save_structures();
    }

    /// 辞書が変わったあと(画面で保存・外で書き換え): 合うテンプレートの無い構造を消し、未注釈の物を裏で注釈する。
    pub(super) fn after_dictionary_change(&mut self) {
        if self.annotations.structures.prune(&self.data.dictionary.templates) > 0 {
            self.save_structures();
        }
        self.queue_annotations();
    }
}
