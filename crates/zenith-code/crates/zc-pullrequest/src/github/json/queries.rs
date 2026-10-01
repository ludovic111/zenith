//! The GraphQL documents, `gh --json` field lists and request bodies of
//! `gitHubPullRequestJson.ts`, byte for byte (the golden test compares every one against the TS
//! exports). Template literals that interpolate a module constant are spelled with `concat!` and
//! the macros below; those that interpolate a caller's value are functions.

use serde::Serialize;
use serde_json::{Map, Value};
use zc_contracts::{PullRequestDiffSide, PullRequestReviewCommentDraft, PullRequestReviewPosition, PullRequestReviewVerdict, PullRequestReviewerKind};
use zc_sourcecontrol::util::js_trim;

use crate::provider::ReviewerRef;

/// `GRAPHQL_PAGE_SIZE`: GitHub's own ceiling on a connection page.
macro_rules! graphql_page_size {
    () => {
        "100"
    };
}

/// `REACTION_GROUPS_FIELDS`: a reaction group as every reactable node reports it, with at most
/// `REACTORS_PER_GROUP` (10) of its people named.
macro_rules! reaction_groups_fields {
    () => {
        "reactionGroups {
  content
  viewerHasReacted
  reactors(first: 10) {
    totalCount
    nodes {
      ... on User { login }
      ... on Bot { login }
      ... on Organization { login }
      ... on Mannequin { login }
    }
  }
}"
    };
}

macro_rules! pull_request_list_json_fields {
    () => {
        "number,title,url,author,headRefName,baseRefName,state,isDraft,mergeable,reviewDecision,additions,deletions,createdAt,updatedAt,mergedAt,reviewRequests,latestReviews,labels,statusCheckRollup"
    };
}

/// `GRAPHQL_PAGE_SIZE`.
const GRAPHQL_PAGE_SIZE: i64 = 100;

/// The ceiling on `search`, which refuses anything larger with EXCESSIVE_PAGINATION.
pub const PULL_REQUEST_SEARCH_MAX_ROWS: i64 = GRAPHQL_PAGE_SIZE;

/// Resolves a listing's authors to avatars, which no `gh` JSON field carries.
pub const ACTOR_AVATARS_GRAPHQL_QUERY: &str = "query($ids: [ID!]!) {
  nodes(ids: $ids) {
    ... on User { login avatarUrl }
    ... on Bot { login avatarUrl }
  }
}";

pub const PULL_REQUEST_LIST_JSON_FIELDS: &str = pull_request_list_json_fields!();

pub const PULL_REQUEST_DETAIL_JSON_FIELDS: &str = concat!(
    pull_request_list_json_fields!(),
    ",body,changedFiles,closedAt,isCrossRepository,headRepositoryOwner,headRefOid,autoMergeRequest"
);

/// Pull refs let the comparison share the detail read without first resolving a fork branch.
pub const PULL_REQUEST_CORE_GRAPHQL_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!, $headRef: String!) {
  repository(owner: $owner, name: $name) {
    mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed viewerPermission
    pullRequest(number: $number) {
      number title url body state isDraft mergeable reviewDecision
      additions deletions changedFiles createdAt updatedAt mergedAt closedAt
      headRefName baseRefName headRefOid isCrossRepository
      headRepositoryOwner { login }
      author { login avatarUrl ... on User { id name } }
      autoMergeRequest { mergeMethod }
      viewerCanUpdate viewerDidAuthor viewerCanUpdateBranch
      baseRef { compare(headRef: $headRef) { behindBy } }
      reviewRequests(first: 100) {
        nodes { requestedReviewer { ... on User { login name } ... on Bot { login } ... on Team { slug name } } }
      }
      labels(first: 100) { nodes { name color } }
      commits(last: 1) {
        nodes { commit { statusCheckRollup { contexts(first: 100) {
          nodes {
            __typename
            ... on StatusContext { context state targetUrl createdAt description }
            ... on CheckRun {
              name status conclusion startedAt completedAt detailsUrl
              checkSuite { workflowRun { workflow { name } } }
            }
          }
          pageInfo { hasNextPage }
        } } } }
      }
    }
  }
}";

pub const PULL_REQUEST_PREVIEW_GRAPHQL_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      number title url state isDraft createdAt
      author { login avatarUrl ... on User { name } }
    }
  }
}";

pub const PULL_REQUEST_ACTIVITY_JSON_FIELDS: &str = "author,comments,reviews,commits";

/// `pullRequestSearchGraphQlQuery`: every repository of a host in one read. The row count
/// (clamped to `1..=PULL_REQUEST_SEARCH_MAX_ROWS`) is written into the document; `additions` and
/// `deletions` are left out (they nearly double the read) and read afterwards by
/// [`build_pull_request_stats_graph_ql_query`]. The inner `first` bounds are bounds, not pages.
pub fn pull_request_search_graph_ql_query(rows: i64, include_stacks: bool) -> String {
    let first = rows.clamp(1, PULL_REQUEST_SEARCH_MAX_ROWS);
    let stacks = if include_stacks {
        "stack { number size baseRefName } stackEntry { position }"
    } else {
        ""
    };
    format!(
        "query($q: String!) {{
  search(query: $q, type: ISSUE, first: {first}) {{
    pageInfo {{ hasNextPage }}
    nodes {{
      ... on PullRequest {{
        {stacks}
        number
        title
        url
        author {{ __typename login avatarUrl ... on User {{ name }} }}
        headRefName
        baseRefName
        state
        isDraft
        mergeable
        reviewDecision
        latestReviews(first: 20) {{ nodes {{ state author {{ login }} }} }}
        createdAt
        updatedAt
        mergedAt
        repository {{ nameWithOwner }}
        reviewRequests(first: 20) {{ nodes {{ requestedReviewer {{ ... on User {{ login }} }} }} }}
        labels(first: 20) {{ nodes {{ name color }} }}
        commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state }} }} }} }}
      }}
    }}
  }}
}}"
    )
}

/// One page of review threads (ten comments each; longer threads are paged from their own
/// cursor), the review roster with avatars, the viewer's standing, reactions, dismissals and the
/// newest hundred commits (`last`, since `gh pr view --json commits` loses the newest ones).
pub const REVIEW_THREADS_GRAPHQL_QUERY: &str = concat!(
    "query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: ",
    graphql_page_size!(),
    ", after: $cursor) {
        totalCount
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          diffSide
          comments(first: 10) {
            totalCount
            pageInfo { hasNextPage endCursor }
            nodes { id author { __typename login avatarUrl } body createdAt url ",
    reaction_groups_fields!(),
    " }
          }
        }
      }
      viewerCanUpdate
      viewerDidAuthor
      author { __typename login avatarUrl }
      ",
    reaction_groups_fields!(),
    "
      comments(first: ",
    graphql_page_size!(),
    ") {
        nodes { id author { __typename login avatarUrl } ",
    reaction_groups_fields!(),
    " }
      }
      reviews(first: ",
    graphql_page_size!(),
    ") { nodes { id author { __typename login avatarUrl } ",
    reaction_groups_fields!(),
    " } }
      reviewRequests(first: 50) {
        nodes {
          requestedReviewer {
            ... on User { login name avatarUrl }
            ... on Bot { __typename login avatarUrl }
          }
        }
      }
      latestReviews(first: 50) {
        nodes { state author { __typename login avatarUrl } }
      }
      reviewDismissals: timelineItems(itemTypes: [REVIEW_DISMISSED_EVENT], first: ",
    graphql_page_size!(),
    ") {
        pageInfo { hasNextPage endCursor }
        nodes { ... on ReviewDismissedEvent { dismissalMessage review { id } } }
      }
      commits(last: ",
    graphql_page_size!(),
    ") {
        nodes {
          commit {
            oid
            messageHeadline
            committedDate
            additions
            deletions
            parents(first: 1) { totalCount }
            authors(first: 3) { nodes { name avatarUrl user { login } } }
          }
        }
      }
    }
  }
}"
);

/// The rest of one thread's conversation, paged from the thread node itself.
pub const REVIEW_THREAD_COMMENTS_GRAPHQL_QUERY: &str = concat!(
    "query($owner: String!, $name: String!, $number: Int!, $threadId: ID!, $cursor: String) {
  viewer { login }
  repository(owner: $owner, name: $name) { pullRequest(number: $number) { id } }
  node(id: $threadId) {
    ... on PullRequestReviewThread {
      pullRequest { id }
      comments(first: ",
    graphql_page_size!(),
    ", after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { id author { __typename login avatarUrl } body createdAt url ",
    reaction_groups_fields!(),
    " }
      }
    }
  }
}"
);

pub const REVIEW_THREAD_REPLY_GRAPHQL_MUTATION: &str = "mutation($threadId: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: { pullRequestReviewThreadId: $threadId, body: $body }) {
    comment { id }
  }
}";

/// The pull request's own node id, which a reaction on its description is addressed by.
pub const PULL_REQUEST_NODE_ID_GRAPHQL_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) { pullRequest(number: $number) { id } }
}";

/// Where a client-given reaction subject actually hangs, read before a mutation reaches it.
pub const REACTION_SUBJECT_PULL_REQUEST_GRAPHQL_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!, $subjectId: ID!) {
  repository(owner: $owner, name: $name) { pullRequest(number: $number) { id } }
  node(id: $subjectId) {
    id
    ... on IssueComment { pullRequest { id } }
    ... on PullRequestReviewComment { pullRequest { id } }
    ... on PullRequestReview { pullRequest { id } }
  }
}";

pub const ADD_REACTION_GRAPHQL_MUTATION: &str = "mutation($subjectId: ID!, $content: ReactionContent!) {
  addReaction(input: { subjectId: $subjectId, content: $content }) { reaction { content } }
}";

pub const REMOVE_REACTION_GRAPHQL_MUTATION: &str = "mutation($subjectId: ID!, $content: ReactionContent!) {
  removeReaction(input: { subjectId: $subjectId, content: $content }) { reaction { content } }
}";

pub const RESOLVE_REVIEW_THREAD_GRAPHQL_MUTATION: &str = "mutation($threadId: ID!) {
  resolveReviewThread(input: { threadId: $threadId }) { thread { isResolved } }
}";

pub const UNRESOLVE_REVIEW_THREAD_GRAPHQL_MUTATION: &str = "mutation($threadId: ID!) {
  unresolveReviewThread(input: { threadId: $threadId }) { thread { isResolved } }
}";

/// Rewrites the title, the description, or both: a variable left unsent leaves its field as it was.
pub const UPDATE_PULL_REQUEST_GRAPHQL_MUTATION: &str = "mutation($pullRequestId: ID!, $title: String, $body: String) {
  updatePullRequest(input: { pullRequestId: $pullRequestId, title: $title, body: $body }) {
    pullRequest { id }
  }
}";

/// Creates a new pull request that reverses a merged pull request.
pub const REVERT_PULL_REQUEST_GRAPHQL_MUTATION: &str = "mutation($pullRequestId: ID!) {
  revertPullRequest(input: { pullRequestId: $pullRequestId }) {
    revertPullRequest { id }
  }
}";

/// The two comment mutations spell their variable the same, so one set of variables serves both.
pub const UPDATE_ISSUE_COMMENT_GRAPHQL_MUTATION: &str = "mutation($commentId: ID!, $body: String!) {
  updateIssueComment(input: { id: $commentId, body: $body }) { issueComment { id } }
}";

pub const UPDATE_REVIEW_COMMENT_GRAPHQL_MUTATION: &str = "mutation($commentId: ID!, $body: String!) {
  updatePullRequestReviewComment(input: { pullRequestReviewCommentId: $commentId, body: $body }) {
    pullRequestReviewComment { id }
  }
}";

/// The dismissal events past the page the thread read carries.
pub const REVIEW_DISMISSALS_GRAPHQL_QUERY: &str = concat!(
    "query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      timelineItems(itemTypes: [REVIEW_DISMISSED_EVENT], first: ",
    graphql_page_size!(),
    ", after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { ... on ReviewDismissedEvent { dismissalMessage review { id } } }
      }
    }
  }
}"
);

/// Where the branch stands against its base (counted commits, not `mergeStateStatus`), and
/// whether this viewer may move it. `headRef` is qualified `owner:branch` for forks.
pub const BASE_COMPARISON_GRAPHQL_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!, $headRef: String!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      viewerCanUpdateBranch
      baseRef {
        compare(headRef: $headRef) {
          behindBy
        }
      }
    }
  }
}";

/// Who a review may be asked of (`assignableUsers`, which a reader without push access can still
/// list) and who it has already been asked of, teams included so a request can be taken back.
pub const REVIEWER_CANDIDATES_GRAPHQL_QUERY: &str = concat!(
    "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    assignableUsers(first: ",
    graphql_page_size!(),
    ") {
      pageInfo { hasNextPage }
      nodes { login name avatarUrl }
    }
    pullRequest(number: $number) {
      author { login }
      reviewRequests(first: ",
    graphql_page_size!(),
    ") {
        nodes {
          requestedReviewer {
            ... on User { login name avatarUrl }
            ... on Team { slug name avatarUrl }
            ... on Bot { login avatarUrl }
          }
        }
      }
    }
  }
}"
);

pub const LABEL_CANDIDATES_GRAPHQL_QUERY: &str = concat!(
    "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    labels(first: ",
    graphql_page_size!(),
    ", orderBy: { field: NAME, direction: ASC }) {
      pageInfo { hasNextPage }
      nodes { name color description }
    }
    pullRequest(number: $number) {
      labels(first: ",
    graphql_page_size!(),
    ") { nodes { name } }
    }
  }
}"
);

/// Core detail and write checks share one read of permissions and merge settings.
pub const VIEWER_PERMISSIONS_GRAPHQL_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed viewerPermission
    pullRequest(number: $number) { viewerCanUpdate viewerDidAuthor }
  }
}";

/// Which files of a pull request the signed-in account has cleared (GraphQL only).
pub const PULL_REQUEST_FILES_VIEWED_GRAPHQL_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      files(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { path viewerViewedState }
      }
    }
  }
}";

/// `PULL_REQUEST_SUMMARY_SELECTION`: the fields a linked thread keeps current.
const PULL_REQUEST_SUMMARY_SELECTION: &str = concat!(
    "number title url state isDraft mergeable reviewDecision additions deletions changedFiles ",
    "updatedAt mergedAt closedAt headRefName baseRefName ",
    "author { __typename login avatarUrl ... on User { name } } ",
    "latestReviews(first: 20) { nodes { state author { login } } } ",
    "commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }"
);

/// A GraphQL request as `gh api graphql --input -` takes it: variables travel in the document,
/// never in argv. Variables keep the order they were given in (a repeated name keeps its first
/// place and its last value, as an object spread does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphQlRequest {
    pub query: String,
    pub variables: Vec<(String, String)>,
}

/// `encodeGraphQlRequestJson`: `{"query": …, "variables": {…}}`.
pub fn encode_graph_ql_request_json<K: AsRef<str>, V: AsRef<str>>(query: &str, variables: &[(K, V)]) -> String {
    let mut encoded = Map::new();
    for (name, value) in variables {
        encoded.insert(name.as_ref().to_owned(), Value::String(value.as_ref().to_owned()));
    }
    let mut request = Map::new();
    request.insert("query".into(), Value::String(query.to_owned()));
    request.insert("variables".into(), Value::Object(encoded));
    Value::Object(request).to_string()
}

/// `gitHubReviewPosition`: where one draft comment lands, as GitHub's REST API names it.
fn git_hub_review_position(position: &PullRequestReviewPosition) -> (i64, &'static str) {
    match position {
        PullRequestReviewPosition::Added(added) => (added.new_line, "RIGHT"),
        PullRequestReviewPosition::Deleted(deleted) => (deleted.old_line, "LEFT"),
        PullRequestReviewPosition::Context(context) => match context.side {
            PullRequestDiffSide::Left => (context.old_line, "LEFT"),
            PullRequestDiffSide::Right => (context.new_line, "RIGHT"),
        },
    }
}

#[derive(Serialize)]
struct ReviewSubmission<'a> {
    event: &'static str,
    body: &'a str,
    comments: Vec<ReviewSubmissionComment<'a>>,
}

#[derive(Serialize)]
struct ReviewSubmissionComment<'a> {
    path: &'a str,
    line: i64,
    side: &'static str,
    body: &'a str,
}

/// `buildReviewSubmissionJson`: the whole review as one `POST …/pulls/{number}/reviews` body,
/// which is how GitHub keeps it invisible until sent.
pub fn build_review_submission_json(verdict: PullRequestReviewVerdict, body: &str, comments: &[PullRequestReviewCommentDraft]) -> String {
    let event = match verdict {
        PullRequestReviewVerdict::Comment => "COMMENT",
        PullRequestReviewVerdict::Approve => "APPROVE",
        PullRequestReviewVerdict::RequestChanges => "REQUEST_CHANGES",
    };
    let submission = ReviewSubmission {
        event,
        body,
        comments: comments
            .iter()
            .map(|comment| {
                let (line, side) = git_hub_review_position(&comment.position);
                ReviewSubmissionComment {
                    path: &comment.path,
                    line,
                    side,
                    body: &comment.body,
                }
            })
            .collect(),
    };
    serde_json::to_string(&submission).expect("a review submission always encodes")
}

/// `buildReviewerRequestJson`: the body of `POST`/`DELETE …/requested_reviewers`, people and
/// teams in their two lists (both always sent).
pub fn build_reviewer_request_json(reviewers: &[ReviewerRef]) -> String {
    let pick = |kind: PullRequestReviewerKind| -> Vec<&str> {
        reviewers
            .iter()
            .filter(|reviewer| reviewer.kind == kind)
            .map(|reviewer| reviewer.id.as_str())
            .collect()
    };
    serde_json::json!({
        "reviewers": pick(PullRequestReviewerKind::User),
        "team_reviewers": pick(PullRequestReviewerKind::Team),
    })
    .to_string()
}

/// `buildLabelRequestJson`: the body of `POST …/issues/{number}/labels`.
pub fn build_label_request_json<S: AsRef<str>>(labels: &[S]) -> String {
    serde_json::json!({ "labels": labels.iter().map(AsRef::as_ref).collect::<Vec<_>>() }).to_string()
}

/// `REPOSITORY_PART`: what a repository selector may hold before it is written into a GraphQL
/// document unquoted (`^[A-Za-z0-9._-]+$`).
fn is_repository_part(part: &str) -> bool {
    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// `Number.isSafeInteger(n) && n > 0`.
fn is_positive_safe_integer(number: i64) -> bool {
    number > 0 && number <= 9_007_199_254_740_991
}

/// `owner/name` split and checked, `None` for anything GraphQL cannot be handed unquoted.
fn repository_parts(repository: &str) -> Option<(&str, &str)> {
    let mut parts = js_trim(repository).split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() || !is_repository_part(owner) || !is_repository_part(name) {
        return None;
    }
    Some((owner, name))
}

/// The aliased lookups of a batch, one per `(repository, number)`, or `None` as soon as one
/// cannot be written into a document.
fn aliased_selections<S: AsRef<str>>(change_requests: &[(S, i64)], selection: &str) -> Option<String> {
    if change_requests.is_empty() {
        return None;
    }
    let mut selections = Vec::with_capacity(change_requests.len());
    for (index, (repository, number)) in change_requests.iter().enumerate() {
        let (owner, name) = repository_parts(repository.as_ref())?;
        if !is_positive_safe_integer(*number) {
            return None;
        }
        selections.push(format!(
            "  s{index}: repository(owner: \"{owner}\", name: \"{name}\") {{ pullRequest(number: {number}) {{ {selection} }} }}"
        ));
    }
    Some(selections.join("\n"))
}

/// `buildPullRequestStatsGraphQlQuery`: line counts for rows a listing already handed over, one
/// aliased lookup (`s<index>`) per `(repository, number)`. `None` for an empty request or any
/// selector that is not a plain repository and positive number, which the caller reports
/// rather than sends.
pub fn build_pull_request_stats_graph_ql_query<S: AsRef<str>>(change_requests: &[(S, i64)]) -> Option<String> {
    let selections = aliased_selections(change_requests, "additions deletions")?;
    Some(format!("query {{\n{selections}\n}}"))
}

/// `buildPullRequestStackMembershipsGraphQlQuery`: stack membership for the visible rows of a
/// per-repository listing; `None` as for the stats query.
pub fn build_pull_request_stack_memberships_graph_ql_query(repository: &str, numbers: &[i64]) -> Option<String> {
    let change_requests: Vec<(&str, i64)> = numbers.iter().map(|number| (repository, *number)).collect();
    let selections = aliased_selections(&change_requests, "stack { number size baseRefName } stackEntry { position }")?;
    Some(format!("query PullRequestStackMemberships {{\n{selections}\n}}"))
}

/// `buildPullRequestSummariesGraphQlQuery`: summaries for pull requests anywhere on one host,
/// checked and written into the document the way the stats query is (`None` = do not send).
pub fn build_pull_request_summaries_graph_ql_query<S: AsRef<str>>(change_requests: &[(S, i64)]) -> Option<String> {
    let selections = aliased_selections(change_requests, PULL_REQUEST_SUMMARY_SELECTION)?;
    Some(format!("query PullRequestSummaries {{\n{selections}\n}}"))
}

/// `buildSetFilesViewedGraphQlMutation`: one document clearing and restoring every ticked file
/// (`(path, viewed)`), one alias (`f<index>`) each, run in write order so the last word about a
/// path sticks. Paths travel as variables (`path<index>`), never in the document;
/// `$pullRequestId` is the caller's to add. `None` for no file.
pub fn build_set_files_viewed_graph_ql_mutation<S: AsRef<str>>(files: &[(S, bool)]) -> Option<GraphQlRequest> {
    if files.is_empty() {
        return None;
    }
    let parameters = (0..files.len()).map(|index| format!("$path{index}: String!")).collect::<Vec<_>>().join(", ");
    let fields = files
        .iter()
        .enumerate()
        .map(|(index, (_, viewed))| {
            let mutation = if *viewed { "markFileAsViewed" } else { "unmarkFileAsViewed" };
            format!("  f{index}: {mutation}(input: {{ pullRequestId: $pullRequestId, path: $path{index} }}) {{ clientMutationId }}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(GraphQlRequest {
        query: format!("mutation($pullRequestId: ID!, {parameters}) {{\n{fields}\n}}"),
        variables: files
            .iter()
            .enumerate()
            .map(|(index, (path, _))| (format!("path{index}"), path.as_ref().to_owned()))
            .collect(),
    })
}
