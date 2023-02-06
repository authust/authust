create table users
(
    uid                  uuid      default gen_random_uuid() primary key not null,
    name                 varchar(32) unique                              not null,
    email                varchar(64) unique,
    display_name         varchar(32),
    password             varchar(255)                                    not null,
    password_change_date timestamp default now()                         not null
);

create table sessions
(
    uid     char(96) not null unique,
    user_id uuid     null references users,
    primary key (uid, user_id)
);

create type policy_kind as enum ('password_expiry', 'password_strength', 'expression');

create table password_expiration_policies
(
    uid     serial primary key,
    max_age int8 not null
);

create table password_strength_policies
(
    uid serial primary key
);

create table expression_policies
(
    uid serial primary key
);

create table policies
(
    uid                 serial primary key,
    slug                varchar(128) not null unique,
    kind                policy_kind  not null,
    password_expiration int4 references password_expiration_policies,
    password_strength   int4 references password_strength_policies,
    expression          int4 references expression_policies
);

create unique index policy_slug ON policies (slug);

create table policy_bindings
(
    uid           serial primary key,
    enabled       bool not null,
    negate_result bool not null,
    policy        int4 not null references policies
);

create type prompt_kind as enum ('username', 'email', 'password', 'text', 'text_read_only', 'signed_number', 'unsigned_number', 'checkbox', 'switch', 'date', 'date_time', 'seperator', 'static', 'locale');

create table prompts
(
    uid         serial primary key,
    field_key   varchar(32) not null,
    label       varchar(32) not null,
    kind        prompt_kind not null,
    placeholder varchar(128),
    required    bool        not null,
    help_text   varchar(128)
);

create type stage_kind as enum ('deny', 'prompt', 'identification', 'user_login', 'user_logout', 'user_write', 'password', 'consent');

create type consent_mode as enum ('always', 'once', 'until');

create table consent_stages
(
    uid   serial primary key,
    mode  consent_mode not null,
    until int8
);

create table stages
(
    uid                           serial primary key,
    slug                          varchar(128) not null unique,
    kind                          stage_kind   not null,
    timeout                       int8         not null,
    identification_password_stage int4 references stages,
    consent_stage                 int4 references consent_stages
);

create table identification_stages
(
    uid            serial primary key,
    password_stage int4 references stages on delete set null
);

create table stage_prompt_bindings
(
    prompt   int4 not null references prompts,
    stage    int4 not null references stages,
    ordering int2 not null,
    primary key (prompt, stage)
);

create type authentication_requirement as enum ('required', 'none', 'superuser', 'ignored');
create type flow_designation as enum ('authentication');

create table flows
(
    uid            serial primary key,
    slug           varchar(128)               not null unique,
    title          varchar(128)               not null unique,
    designation    flow_designation           not null,
    authentication authentication_requirement not null

);

create table flow_entries
(
    uid      serial primary key,
    flow     int4 not null references flows,
    stage    int4 not null references stages,
    ordering int2 not null
);

create unique index on flow_entries (flow, stage, ordering);

create table flow_bindings
(
    policy        int4 not null references policies,
    flow          int4 references flows,
    entry         int4 references flow_entries,
    group_binding uuid,
    user_binding  uuid references users,

    ordering      int2 not null,
    enabled       bool not null,
    negate_result bool not null
);


create table providers
(
    uid          serial primary key,
    slug         varchar(64) not null,
    display_name varchar(64) not null
);

create table applications
(
    uid          serial primary key,
    slug         varchar(64) not null,
    display_name varchar(64) not null,
    provider     int4        not null references providers
);