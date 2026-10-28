//! Reading what was said before: the whole reply tree of a job, and a search
//! over the jobs an agent sent or received. Both only read, so nothing is
//! delivered and no `updated_seq` moves: the other party's outbox never sees them.

use rusqlite::params;
use serde_json::{Value, json};

use super::{
    JOB_COLUMNS, Job, Kind, PREVIEW_COLUMNS, SUMMARY_COLUMNS, State, Store, View, job_from_row, limits,
    load_attachments, load_job, not_yours,
};
use crate::error::ApiResult;
use crate::types::{AgentName, Cursor, JobId};

// `reply_to` always names an older job (it must exist when the reply is made,
// and ids are never reused), so neither walk below can loop; their depth bound
// of 10000 only guards against a database edited by hand.

/// The oldest ancestor of `?1` that still exists, by `reply_to`.
const ROOT_SQL: &str = "WITH RECURSIVE up(uid, ureply, n) AS ( \
                            SELECT id, reply_to, 0 FROM jobs WHERE id = ?1 \
                            UNION ALL \
                            SELECT j.id, j.reply_to, up.n + 1 FROM jobs j JOIN up ON j.id = up.ureply \
                            WHERE up.n < 10000) \
                        SELECT uid FROM up ORDER BY n DESC LIMIT 1";

/// Every job under root `?1` with its depth, through the `jobs_reply_to` index.
/// The column names differ from the `jobs` ones so the selects below need no prefix.
const TREE: &str = "WITH RECURSIVE tree(tid, tdepth) AS ( \
                        SELECT ?1, 0 \
                        UNION ALL \
                        SELECT j.id, tree.tdepth + 1 FROM jobs j JOIN tree ON j.reply_to = tree.tid \
                        WHERE tree.tdepth < 10000)";

pub struct ThreadQuery {
    /// `View::Summary` as in the inbox, or `View::Full` with whole bodies.
    pub view: View,
    pub limit: u32,
    /// Only jobs created after this one.
    pub after: Option<Cursor>,
}

/// One page of a thread, oldest first, in its JSON answer form.
pub struct ThreadPage {
    pub root: JobId,
    pub jobs: Vec<Value>,
    /// Jobs of the thread between other agents, never shown and only counted.
    pub omitted: u64,
    /// Set when jobs are left for a next page, by `limit` or by the size cap.
    pub next: Option<Cursor>,
}

impl ThreadPage {
    pub fn to_json(&self) -> Value {
        json!({
            "root": self.root.to_string(),
            "jobs": self.jobs,
            "omitted": self.omitted,
            "truncated": self.next.is_some(),
            "next": self.next.map(|c| c.to_string()),
        })
    }
}

pub struct SearchQuery {
    /// Matched as a substring of title or body; the store escapes it for `LIKE`.
    pub text: Option<String>,
    pub from: Option<AgentName>,
    pub to: Option<AgentName>,
    pub kind: Option<Kind>,
    pub state: Option<State>,
    /// Unix seconds: created at or after.
    pub since: Option<i64>,
    /// Unix seconds: created before.
    pub until: Option<i64>,
    pub limit: u32,
    /// Only jobs older than this one.
    pub after: Option<Cursor>,
}

pub struct SearchPage {
    pub jobs: Vec<Job>,
    pub next: Option<Cursor>,
}

impl SearchPage {
    pub fn to_json(&self) -> Value {
        json!({
            "jobs": self.jobs.iter().map(|j| j.to_json(View::Preview)).collect::<Vec<_>>(),
            "next": self.next.map(|c| c.to_string()),
        })
    }
}

fn cursor_of(job: &Job) -> Cursor {
    Cursor {
        created_at: job.created_at,
        id: job.id.get(),
    }
}

/// `text` as a `LIKE` pattern matching it anywhere, with `%`, `_` and the escape
/// character itself taken literally (`ESCAPE '\'`).
fn like_pattern(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len() + 8);
    pattern.push('%');
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
}

impl Store {
    /// The conversation `id` belongs to: its root by `reply_to` and every reply
    /// below it, oldest first. The caller must be a party of `id`; jobs of the
    /// tree between other agents are counted in `omitted`, never shown.
    pub fn thread(&self, me: &AgentName, id: JobId, query: &ThreadQuery) -> ApiResult<ThreadPage> {
        let conn = self.db();
        let start = load_job(&conn, id, View::Summary)?;
        if !start.is_party(me) {
            return Err(not_yours(&start));
        }
        let root = JobId::new(conn.query_row(ROOT_SQL, [id.get()], |row| row.get(0))?);
        let omitted: u64 = conn.query_row(
            &format!(
                "{TREE} SELECT count(*) FROM jobs JOIN tree ON id = tid WHERE from_agent <> ?2 AND to_agent <> ?2"
            ),
            params![root.get(), me.as_str()],
            |row| row.get(0),
        )?;
        let columns = match query.view {
            View::Full => JOB_COLUMNS,
            View::Summary | View::Preview => SUMMARY_COLUMNS,
        };
        let mut stmt = conn.prepare(&format!(
            "{TREE} SELECT {columns}, tdepth FROM jobs JOIN tree ON id = tid \
             WHERE (from_agent = ?2 OR to_agent = ?2) AND (?3 IS NULL OR (created_at, id) > (?3, ?4)) \
             ORDER BY created_at, id LIMIT ?5"
        ))?;
        let after = query.after.map(|c| (c.created_at, c.id));
        let rows = stmt.query_map(
            params![
                root.get(),
                me.as_str(),
                after.map(|a| a.0),
                after.map(|a| a.1),
                query.limit.saturating_add(1)
            ],
            |row| Ok((job_from_row(row)?, row.get::<_, i64>(24)?)),
        )?;
        // Rows are read one at a time, so a page of whole reports never sits in memory past the cap.
        let budget = limits::THREAD_BYTES - 1024;
        let mut used = 0;
        let mut jobs = Vec::with_capacity(query.limit.min(limits::LIST_MAX) as usize);
        let mut last = None;
        let mut more = false;
        for row in rows {
            let (mut job, depth) = row?;
            if jobs.len() == query.limit as usize {
                more = true;
                break;
            }
            job.attachments = load_attachments(&conn, job.id)?;
            let mut entry = job.to_json(query.view);
            entry["depth"] = Value::from(depth);
            let mut size = entry.to_string().len();
            if used + size > budget {
                if !jobs.is_empty() {
                    more = true;
                    break;
                }
                // A first job too big for the cap alone comes as a summary, so every page makes progress.
                entry = load_job(&conn, job.id, View::Summary)?.to_json(View::Summary);
                entry["depth"] = Value::from(depth);
                size = entry.to_string().len();
            }
            used += size;
            last = Some(cursor_of(&job));
            jobs.push(entry);
        }
        Ok(ThreadPage {
            root,
            jobs,
            omitted,
            next: if more { last } else { None },
        })
    }

    /// Jobs the caller sent or received that match `query`, newest first, as previews.
    pub fn search(&self, me: &AgentName, query: &SearchQuery) -> ApiResult<SearchPage> {
        let conn = self.db();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {PREVIEW_COLUMNS} FROM jobs WHERE (from_agent = ?1 OR to_agent = ?1) \
             AND (?2 IS NULL OR title LIKE ?2 ESCAPE '\\' OR body LIKE ?2 ESCAPE '\\') \
             AND (?3 IS NULL OR from_agent = ?3) AND (?4 IS NULL OR to_agent = ?4) \
             AND (?5 IS NULL OR kind = ?5) AND (?6 IS NULL OR state = ?6) \
             AND (?7 IS NULL OR created_at >= ?7) AND (?8 IS NULL OR created_at < ?8) \
             AND (?9 IS NULL OR (created_at, id) < (?9, ?10)) \
             ORDER BY created_at DESC, id DESC LIMIT ?11"
        ))?;
        let after = query.after.map(|c| (c.created_at, c.id));
        let rows = stmt.query_map(
            params![
                me.as_str(),
                query.text.as_deref().map(like_pattern),
                query.from.as_ref().map(AgentName::as_str),
                query.to.as_ref().map(AgentName::as_str),
                query.kind.map(Kind::as_str),
                query.state.map(State::as_str),
                query.since,
                query.until,
                after.map(|a| a.0),
                after.map(|a| a.1),
                query.limit.saturating_add(1)
            ],
            job_from_row,
        )?;
        let mut jobs = Vec::with_capacity(query.limit.min(limits::LIST_MAX) as usize);
        let mut more = false;
        for row in rows {
            let mut job = row?;
            if jobs.len() == query.limit as usize {
                more = true;
                break;
            }
            job.attachments = load_attachments(&conn, job.id)?;
            jobs.push(job);
        }
        let next = if more { jobs.last().map(cursor_of) } else { None };
        Ok(SearchPage { jobs, next })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{TempDir, agent, delivery, store, task};
    use super::super::{Move, NewJob, Outcome};
    use super::*;
    use crate::types::Seq;

    fn thread_query(view: View, limit: u32) -> ThreadQuery {
        ThreadQuery {
            view,
            limit,
            after: None,
        }
    }

    fn search_query() -> SearchQuery {
        SearchQuery {
            text: None,
            from: None,
            to: None,
            kind: None,
            state: None,
            since: None,
            until: None,
            limit: 50,
            after: None,
        }
    }

    fn ids(page: &ThreadPage) -> Vec<String> {
        page.jobs
            .iter()
            .map(|j| j["id"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn reply(kind: Kind, to: &str, re: JobId, body: &str) -> NewJob {
        NewJob {
            reply_to: Some(re),
            ..delivery(kind, to, body)
        }
    }

    #[test]
    fn a_thread_is_the_same_from_any_job_and_either_side() {
        let (_dir, s) = store();
        let (linux, windows) = (agent("linux"), agent("windows"));
        let root = s.create_job(&linux, task("windows")).expect("task");
        s.move_job(&windows, root.id, Move::Receive, None).expect("receive");
        s.move_job(&windows, root.id, Move::Accept, None).expect("accept");
        let comment = s
            .create_job(&linux, reply(Kind::Comment, "windows", root.id, "one more thing"))
            .expect("comment");
        let question = s
            .create_job(&windows, reply(Kind::Message, "linux", comment.id, "which branch?"))
            .expect("question");
        let answer = s
            .create_job(&linux, reply(Kind::Message, "windows", question.id, "main"))
            .expect("answer");
        let result = NewJob {
            outcome: Some(Outcome::Done),
            ..reply(Kind::Result, "linux", root.id, "done")
        };
        let result = s.create_job(&windows, result).expect("result");
        let elsewhere = s.create_job(&linux, task("windows")).expect("other thread");

        let all = [root.id, comment.id, question.id, answer.id, result.id].map(|j| j.to_string());
        for (me, start) in [(&linux, answer.id), (&windows, root.id), (&windows, result.id)] {
            let page = s.thread(me, start, &thread_query(View::Summary, 50)).expect("thread");
            assert_eq!(page.root, root.id);
            assert_eq!(ids(&page), all, "oldest first, from {start} as {me}");
            assert_eq!((page.omitted, page.next), (0, None));
        }
        let page = s
            .thread(&linux, question.id, &thread_query(View::Summary, 50))
            .expect("thread");
        let depths: Vec<i64> = page.jobs.iter().filter_map(|j| j["depth"].as_i64()).collect();
        assert_eq!(depths, [0, 1, 2, 3, 1]);
        assert_eq!(page.jobs[2]["reply_to"], comment.id.to_string());
        assert_eq!(page.jobs[2]["body"], "which branch?", "short messages come whole");
        assert!(!ids(&page).contains(&elsewhere.id.to_string()));
        assert_eq!(page.to_json()["truncated"], false);

        let err = s
            .thread(&agent("mac"), root.id, &thread_query(View::Summary, 50))
            .err()
            .map(|e| e.code);
        assert_eq!(err, Some(crate::error::Code::NotYours));
    }

    #[test]
    fn reading_a_thread_delivers_nothing() {
        let (_dir, s) = store();
        let (linux, windows) = (agent("linux"), agent("windows"));
        let m = s
            .create_job(&linux, delivery(Kind::Notice, "windows", "deploying"))
            .expect("notice");
        let before = s.outbox(&linux, Seq::new(0), 50).expect("outbox").next;
        let page = s.thread(&windows, m.id, &thread_query(View::Full, 50)).expect("thread");
        assert_eq!(page.jobs[0]["state"], "sent");
        let found = s
            .search(
                &windows,
                &SearchQuery {
                    text: Some("deploy".to_owned()),
                    ..search_query()
                },
            )
            .expect("search");
        assert_eq!(found.jobs.len(), 1);
        assert!(
            s.outbox(&linux, Seq::new(before), 50).expect("outbox").jobs.is_empty(),
            "no state change and no updated_seq bump"
        );
        let inbox = s.inbox(&windows, Seq::new(0), None, 50).expect("inbox");
        assert_eq!(inbox.jobs[0].state, State::Received, "still new for the inbox");
    }

    #[test]
    fn foreign_jobs_are_counted_never_shown() {
        let (_dir, s) = store();
        let linux = agent("linux");
        let root = s.create_job(&linux, task("windows")).expect("task");
        // The API cannot put another pair under this tree; an old or edited database could.
        s.db()
            .execute(
                "INSERT INTO jobs (from_agent, to_agent, kind, title, body, tags, reply_to, state, created_at, \
                 updated_at, seq, updated_seq) VALUES ('mac', 'windows', 'message', 'secret plans', 'x', '[]', \
                 ?1, 'sent', ?2, ?2, 1, 1)",
                params![root.id.get(), root.created_at],
            )
            .expect("foreign row");
        let page = s
            .thread(&linux, root.id, &thread_query(View::Full, 50))
            .expect("thread");
        assert_eq!(ids(&page), [root.id.to_string()]);
        assert_eq!(page.omitted, 1);
        assert!(!page.to_json().to_string().contains("secret plans"));
    }

    #[test]
    fn pages_follow_the_limit_and_the_size_cap() {
        let (_dir, s) = store();
        let (linux, windows) = (agent("linux"), agent("windows"));
        let root = s
            .create_job(&linux, delivery(Kind::Message, "windows", "start"))
            .expect("root");
        let mut last = root.id;
        let mut all = vec![root.id.to_string()];
        for n in 0..4 {
            let (me, to) = if n % 2 == 0 {
                (&windows, "linux")
            } else {
                (&linux, "windows")
            };
            let big = NewJob {
                title: format!("part {n}"),
                ..reply(Kind::Report, to, last, &"r".repeat(limits::REPORT_BYTES / 2))
            };
            last = s.create_job(me, big).expect("report").id;
            all.push(last.to_string());
        }
        let mut seen = Vec::new();
        let mut after = None;
        let mut pages = 0;
        loop {
            let query = ThreadQuery {
                view: View::Full,
                limit: 3,
                after,
            };
            let page = s.thread(&linux, root.id, &query).expect("page");
            let json = page.to_json();
            assert!(
                json.to_string().len() <= limits::THREAD_BYTES,
                "page {pages} over the cap"
            );
            seen.extend(ids(&page));
            pages += 1;
            match page.next {
                Some(next) => {
                    assert_eq!(json["truncated"], true);
                    assert_eq!(json["next"], next.to_string());
                    after = Some(next);
                }
                None => break,
            }
        }
        assert_eq!(seen, all, "every job once, in order");
        assert!(pages >= 3, "two half-MiB reports never share a page: {pages}");

        let summaries = s
            .thread(&linux, root.id, &thread_query(View::Summary, 2))
            .expect("page");
        assert_eq!(summaries.jobs.len(), 2);
        assert!(summaries.next.is_some());
        assert_eq!(summaries.jobs[1]["body_truncated"], true);
        assert!(summaries.jobs[1].get("body").is_none(), "reports come as previews");
    }

    #[test]
    fn a_first_job_over_the_cap_comes_as_a_summary() {
        let (_dir, s) = store();
        let linux = agent("linux");
        // JSON escaping turns each control character into six bytes, so a 1 MiB report passes the cap.
        let report = delivery(Kind::Report, "windows", &"\u{1}".repeat(limits::REPORT_BYTES));
        let r = s.create_job(&linux, report).expect("report");
        let page = s.thread(&linux, r.id, &thread_query(View::Full, 50)).expect("page");
        assert_eq!(page.jobs.len(), 1);
        assert!(page.jobs[0].get("body").is_none());
        assert_eq!(page.jobs[0]["body_truncated"], true);
        assert!(page.to_json().to_string().len() <= limits::THREAD_BYTES);
    }

    #[test]
    fn search_escapes_like_and_filters() {
        let (_dir, s) = store();
        let (linux, windows) = (agent("linux"), agent("windows"));
        let percent = s
            .create_job(&linux, delivery(Kind::Message, "windows", "coverage is 100% now"))
            .expect("1");
        let under = s
            .create_job(&windows, delivery(Kind::Message, "linux", "see my_file.rs"))
            .expect("2");
        let back = s
            .create_job(&linux, delivery(Kind::Notice, "windows", "path C:\\temp\\x"))
            .expect("3");
        let task_job = s
            .create_job(
                &linux,
                NewJob {
                    title: "Run the GPU probe".to_owned(),
                    ..task("windows")
                },
            )
            .expect("4");
        let find = |me: &AgentName, q: SearchQuery| -> Vec<JobId> {
            s.search(me, &q).expect("search").jobs.iter().map(|j| j.id).collect()
        };
        let text = |t: &str| SearchQuery {
            text: Some(t.to_owned()),
            ..search_query()
        };
        assert_eq!(find(&linux, text("100%")), [percent.id]);
        assert_eq!(find(&linux, text("%")), [percent.id], "% is literal");
        assert_eq!(find(&linux, text("y_f")), [under.id], "_ is literal");
        assert_eq!(find(&linux, text("_")), [under.id]);
        assert_eq!(find(&linux, text("\\temp\\")), [back.id], "the escape char is literal");
        assert_eq!(
            find(&linux, text("gpu PROBE")),
            [task_job.id],
            "titles, ASCII case folded"
        );
        assert_eq!(find(&linux, text("nothing like it")), []);
        assert_eq!(
            find(&linux, search_query()),
            [task_job.id, back.id, under.id, percent.id],
            "newest first"
        );
        let only = |q: SearchQuery| find(&windows, q);
        assert_eq!(
            only(SearchQuery {
                kind: Some(Kind::Notice),
                ..search_query()
            }),
            [back.id]
        );
        assert_eq!(
            only(SearchQuery {
                from: Some(windows.clone()),
                ..search_query()
            }),
            [under.id]
        );
        assert_eq!(
            only(SearchQuery {
                to: Some(linux.clone()),
                state: Some(State::Sent),
                ..search_query()
            }),
            [under.id]
        );
        assert_eq!(
            only(SearchQuery {
                since: Some(task_job.created_at + 1),
                ..search_query()
            }),
            []
        );
        assert_eq!(
            only(SearchQuery {
                until: Some(percent.created_at),
                ..search_query()
            }),
            [],
            "until is exclusive"
        );
        assert_eq!(find(&agent("mac"), search_query()), [], "only your own jobs");
        let json = s.search(&windows, &text("coverage")).expect("search").to_json();
        assert!(
            json["jobs"][0].get("body").is_none(),
            "previews only, even for short messages"
        );
        assert_eq!(json["jobs"][0]["body_preview"], "coverage is 100% now");
    }

    #[test]
    fn search_cursors_hold_while_jobs_arrive() {
        let (_dir, s) = store();
        let linux = agent("linux");
        let made: Vec<JobId> = (0..5)
            .map(|n| {
                s.create_job(&linux, delivery(Kind::Message, "windows", &format!("m{n}")))
                    .expect("message")
                    .id
            })
            .collect();
        let page = |after| {
            s.search(
                &linux,
                &SearchQuery {
                    limit: 2,
                    after,
                    ..search_query()
                },
            )
            .expect("search")
        };
        let first = page(None);
        assert_eq!(first.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), [made[4], made[3]]);
        // Same second as the jobs already listed, larger id: it must not slip into a later page.
        s.create_job(&linux, delivery(Kind::Message, "windows", "late"))
            .expect("late");
        let second = page(first.next);
        assert_eq!(second.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), [made[2], made[1]]);
        let third = page(second.next);
        assert_eq!(third.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), [made[0]]);
        assert_eq!(third.next, None);
    }

    #[test]
    fn old_databases_gain_the_thread_index() {
        let dir = TempDir::new();
        std::fs::create_dir_all(&dir.0).expect("dir");
        let conn = rusqlite::Connection::open(dir.0.join("racking.db")).expect("db");
        // The first schema: no index on reply_to, no priority or role columns.
        conn.execute_batch(
            "CREATE TABLE jobs (id INTEGER PRIMARY KEY AUTOINCREMENT, from_agent TEXT NOT NULL, \
             to_agent TEXT NOT NULL, kind TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL, \
             tags TEXT NOT NULL, origin TEXT, reply_to INTEGER, outcome TEXT, reason TEXT, state TEXT NOT NULL, \
             created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, seq INTEGER NOT NULL, \
             updated_seq INTEGER NOT NULL) STRICT;
             INSERT INTO jobs VALUES (1, 'linux', 'windows', 'task', 't', 'b', '[]', NULL, NULL, NULL, NULL, \
             'sent', 0, 0, 1, 1);
             INSERT INTO jobs VALUES (2, 'windows', 'linux', 'comment', 'c', 'b', '[]', NULL, 1, NULL, NULL, \
             'sent', 1, 1, 1, 2);",
        )
        .expect("old schema");
        drop(conn);
        let s = Store::open(&dir.0).expect("open");
        let plan: Vec<String> = {
            let conn = s.db();
            let mut stmt = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {TREE} SELECT tid FROM tree"))
                .expect("plan");
            stmt.query_map([1], |row| row.get::<_, String>(3))
                .expect("rows")
                .collect::<Result<_, _>>()
                .expect("plan rows")
        };
        assert!(plan.iter().any(|l| l.contains("jobs_reply_to")), "{plan:?}");
        let page = s
            .thread(&agent("windows"), JobId::new(2), &thread_query(View::Summary, 50))
            .expect("thread");
        assert_eq!(ids(&page), ["j1", "j2"]);
        drop(s);
        Store::open(&dir.0).expect("opening twice is fine");
    }

    #[test]
    fn like_patterns() {
        assert_eq!(like_pattern("a%b_c\\d"), "%a\\%b\\_c\\\\d%");
        assert_eq!(like_pattern("ñ"), "%ñ%");
    }
}
