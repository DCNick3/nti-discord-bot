use chrono::Utc;
use futures::StreamExt;
use redis::aio::MultiplexedConnection;
use redis::{from_redis_value, Value};
use std::collections::{HashMap, HashSet};
use std::default::Default;
use std::fmt::Write;
use std::sync::Arc;

use serenity::async_trait;
use serenity::builder::CreateApplicationCommandOption;
use serenity::client::{Client, Context, EventHandler};
use serenity::framework::standard::{macros::group, StandardFramework};
use serenity::model::gateway::Ready;
use serenity::model::guild::Guild;
use serenity::model::id::{ChannelId, GuildId, UserId};
use serenity::model::interactions::application_command::{
    ApplicationCommandInteractionDataOptionValue, ApplicationCommandOptionType,
};
use serenity::model::interactions::{Interaction, InteractionResponseType};
use serenity::model::prelude::VoiceState;
use serenity::prelude::{TypeMap, TypeMapKey};

const DUMMY_TEAM: i64 = -1;
const DUMMY_TABLE: i64 = 0;

struct Config {
    pub environment: String,
    pub token: String,
    pub app_id: u64,
    pub guild_id: GuildId,
    pub tables: HashMap<i64, ChannelId>,
    pub teams: HashMap<i64, HashSet<UserId>>,
    pub team_names: HashMap<i64, String>,
    pub user_teams: HashMap<UserId, i64>,
    pub team_voice_rooms: HashMap<i64, ChannelId>,
    pub participants: HashSet<UserId>,
    pub blessed_users: HashSet<UserId>,
}

impl Config {
    pub fn user_team(&self, user: UserId) -> Option<i64> {
        self.user_teams.get(&user).cloned()
    }

    pub fn team_table(&self, team_id: i64) -> i64 {
        if team_id >= 1 && team_id <= 5 {
            1
        } else if team_id >= 6 && team_id <= 11 {
            2
        } else {
            DUMMY_TEAM
        }
    }

    pub fn queue_name(&self, table_id: i64) -> String {
        format!("{}:queue:{}", self.environment, table_id)
    }

    pub fn team_to_queue_name(&self) -> String {
        format!("{}:team_to_queue", self.environment)
    }
}

#[group]
struct General;

struct Handler;

async fn table_join(ctx: &Context, team_id: i64, table_id: i64) -> (usize, usize, usize) {
    let data = ctx.data.read().await;
    let config = data.config();
    let voice = data.voice_state();

    let guild = config.guild_id;

    let team: HashSet<UserId> = config
        .teams
        .get(&team_id)
        .unwrap()
        .iter()
        .cloned()
        .collect();
    let table = config.tables.get(&table_id).cloned().unwrap();
    let neutral_channel = config.tables.get(&DUMMY_TABLE).unwrap();

    // for u in team.iter() {
    //     guild.move_member(&ctx, u, table).await.unwrap();
    // }
    //
    // for u in config.participants.iter().filter(|f| !team.contains(f)) {
    //     guild.move_member(&ctx, u, neutral_channel).await.unwrap();
    // }

    let members = HashSet::new();
    let members = voice.members.get(&table_id).unwrap_or(&members);
    let to_kick = members
        .iter()
        .filter(|f| config.participants.contains(f) && !team.contains(f));
    let to_join = team.iter().filter(|f| !members.contains(f));

    let mut people_fail = 0;
    let mut people_in = 0;
    let mut people_out = 0;
    for &u in to_kick {
        let team_id = config.user_team(u).unwrap();
        let channel = config.team_voice_rooms.get(&team_id).unwrap();

        let r = guild.move_member(&ctx, u, channel).await;
        if let Err(r) = r {
            println!("Failed to kick {} from table {}: {:?}", u, table, r);
            people_fail += 1
        } else {
            people_out += 1;
        }
    }
    for u in to_join {
        let r = guild.move_member(&ctx, u, table).await;
        if let Err(r) = r {
            println!("Failed to join {} to table {}: {:?}", u, table, r);
            people_fail += 1
        } else {
            people_in += 1;
        }
    }

    (people_in, people_out, people_fail)
}

async fn get_queue(config: &Config, redis: &mut MultiplexedConnection, table_id: i64) -> Vec<i64> {
    let queue = config.queue_name(table_id);

    redis::Cmd::zrange(queue, 0, 100)
        .query_async(redis)
        .await
        .unwrap()
}

async fn add_to_queue(
    config: &Config,
    redis: &mut MultiplexedConnection,
    table_id: i64,
    team_id: i64,
) -> bool {
    let queue = config.queue_name(table_id);
    let team2queue = config.team_to_queue_name();

    if redis::Cmd::hexists(&team2queue, team_id)
        .query_async(redis)
        .await
        .unwrap()
    {
        return false;
    }

    let _: () = redis::Cmd::hset(&team2queue, team_id, table_id)
        .query_async(redis)
        .await
        .unwrap();

    let _: () = redis::cmd("ZADD")
        .arg(queue)
        .arg("NX")
        .arg(Utc::now().timestamp_millis())
        .arg(team_id)
        .query_async(redis)
        .await
        .unwrap();

    true
}

async fn add_to_smallest_queue(
    config: &Config,
    redis: &mut MultiplexedConnection,
    team_id: i64,
) -> Option<i64> {
    let team2queue = config.team_to_queue_name();

    // I see races...
    // They are everywhere!
    if redis::Cmd::hexists(team2queue, team_id)
        .query_async(redis)
        .await
        .unwrap()
    {
        return None;
    }

    let queues = config.tables.keys();
    let queues = queues.filter(|&&q| q != DUMMY_TABLE).map(|q| async {
        let q = *q;
        let queue_name = config.queue_name(q);

        let mut redis = redis.clone();
        let r: i64 = redis::Cmd::zcount(&queue_name, f64::NEG_INFINITY, f64::INFINITY)
            .query_async(&mut redis)
            .await
            .unwrap();
        (q, r)
    });

    let queues = futures::future::join_all(queues).await;

    let (queue, _) = queues.into_iter().min_by_key(|(_, sz)| *sz).unwrap();

    assert!(add_to_queue(config, redis, queue, team_id).await);

    Some(queue)
}

async fn unqueue(config: &Config, redis: &mut MultiplexedConnection, team_id: i64) -> Option<i64> {
    let team2queue = config.team_to_queue_name();

    let table_id: Option<i64> = redis::Cmd::hget(&team2queue, team_id)
        .query_async(redis)
        .await
        .unwrap();

    if let Some(table_id) = table_id {
        let queue = config.queue_name(table_id);
        let _: () = redis::Cmd::zrem(&queue, team_id)
            .query_async(redis)
            .await
            .unwrap();

        let _: () = redis::Cmd::hdel(&team2queue, team_id)
            .query_async(redis)
            .await
            .unwrap();

        Some(table_id)
    } else {
        None
    }
}

async fn dequeue(config: &Config, redis: &mut MultiplexedConnection, table_id: i64) -> Option<i64> {
    let queue = config.queue_name(table_id);

    let result: redis::Value = redis::Cmd::zpopmin(queue, 1)
        .query_async(redis)
        .await
        .unwrap();

    match &result {
        Value::Bulk(b) => {
            if b.is_empty() {
                return None;
            }
        }
        _ => return None,
    }

    let (team_id, _): (i64, f64) = from_redis_value(&result).unwrap();

    let team2queue = config.team_to_queue_name();

    let _: () = redis::Cmd::hdel(team2queue, team_id)
        .query_async(redis)
        .await
        .unwrap();

    Some(team_id)
}

fn format_queue(config: &Config, queue: &Vec<i64>) -> String {
    let mut res = String::new();

    for (_, team_id) in queue.iter().enumerate() {
        writeln!(
            res,
            "  team {}: {}",
            team_id,
            config.team_names.get(&team_id).unwrap()
        )
        .unwrap();
    }

    if queue.is_empty() {
        res = "  (empty)".to_string();
    }

    res
}

// #[async_trait]
trait ContextExt {
    fn config(&self) -> Arc<Config>;
    fn voice_state(&self) -> &TableVoiceState;
    fn redis(&self) -> MultiplexedConnection;
}

// #[async_trait]
impl ContextExt for TypeMap {
    fn config(&self) -> Arc<Config> {
        // let data = self.data.read().await;
        self.get::<ConfigKey>().unwrap().clone()
    }
    fn voice_state(&self) -> &TableVoiceState {
        // let data = self.data.read().await;
        self.get::<TableVoiceState>().unwrap()
    }
    fn redis(&self) -> MultiplexedConnection {
        self.get::<RedisConnKey>().unwrap().clone()
    }
}

fn table_option(
    option: &mut CreateApplicationCommandOption,
) -> &mut CreateApplicationCommandOption {
    option
        .name("table")
        .description("A table id (1 or 2)")
        .kind(ApplicationCommandOptionType::Integer)
        .add_int_choice("Table #1", 1)
        .add_int_choice("Table #2", 2)
        .required(true)
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);

        let config = {
            let data = ctx.data.read().await;
            data.config()
        };

        let _commands =
            GuildId::set_application_commands(&config.guild_id, &ctx.http, |commands| {
                commands
                    .create_application_command(|command| {
                        command.name("ping").description("A ping command")
                    })
                    .create_application_command(|command| {
                        command
                            .name("tjoin")
                            .description("Move the team to the table")
                            .create_option(table_option)
                            .create_option(|option| {
                                option
                                    .name("team_id")
                                    .description("Team id (1-12)")
                                    .kind(ApplicationCommandOptionType::Integer)
                                    .min_int_value(1)
                                    .max_int_value(12)
                                    .required(true);
                                for i in 1..12 {
                                    option.add_int_choice(
                                        format!("#{}: {}", i, config.team_names.get(&i).unwrap()),
                                        i as i32,
                                    );
                                }
                                option
                            })
                    })
                    .create_application_command(|command| {
                        command
                            .name("tkick")
                            .description("Kick the team currently at the table")
                            .create_option(table_option)
                    })
                    .create_application_command(|command| {
                        command
                            .name("enqueue")
                            .description("Enqueue another person to the table")
                            .create_option(|option| {
                                option
                                    .name("user")
                                    .description("The user to enqueue")
                                    .kind(ApplicationCommandOptionType::User)
                                    .required(true)
                            })
                    })
                    .create_application_command(|command| {
                        command
                            .name("enqueue_me")
                            .description("Enqueue my team for the trial runs please")
                    })
                    .create_application_command(|command| {
                        command
                            .name("enqueue_me_faster")
                            .description("Enqueue my team, use the table with the smallest queue")
                    })
                    .create_application_command(|command| {
                        command
                            .name("unqueue_me")
                            .description("Unqueue my team, I don't want to be called to trial runs")
                    })
                    .create_application_command(|command| {
                        command
                            .name("queue")
                            .description("Show the current state of the queue")
                    })
                    .create_application_command(|command| {
                        command.name("dequeue")
                            .description("Dequeue the team from the specified table queue & move them to the table")
                            .create_option(table_option)
                    })
                    .create_application_command(|command| {
                        command.name("dequeue_nojoin")
                            .description("Dequeue the team from the specified table queue, but do NOT move them to the table")
                            .create_option(table_option)
                    })
            })
            .await
            .unwrap();

        // println!(
        //     "I now have the following guild slash commands: {:#?}",
        //     _commands
        // );
    }

    async fn guild_create(&self, ctx: Context, guild: Guild, _is_new: bool) {
        let config = {
            let data = ctx.data.read().await;
            data.config()
        };

        let mut data = ctx.data.write().await;
        let voice = data.get_mut::<TableVoiceState>().unwrap();

        if guild.id != config.guild_id {
            return;
        }

        println!("Got a guild_create for our configured guild!");

        for (user_id, voice_state) in guild.voice_states {
            if config.participants.contains(&user_id) {
                if let Some(channel) = voice_state.channel_id {
                    if let Some((&table, _)) = config.tables.iter().find(|(_, &c)| c == channel) {
                        let table_members = voice.members.entry(table).or_insert_with(HashSet::new);
                        table_members.insert(user_id);
                    }
                }
            }
        }

        println!("voice state: {:?}", voice.members);
    }

    async fn voice_state_update(
        &self,
        ctx: Context,
        guild: Option<GuildId>,
        old: Option<VoiceState>,
        new: VoiceState,
    ) {
        let config = {
            let data = ctx.data.read().await;
            data.config()
        };

        if Some(config.guild_id) != guild || !config.participants.contains(&new.user_id) {
            return;
        }

        let mut data = ctx.data.write().await;
        let voice = data.get_mut::<TableVoiceState>().unwrap();

        if let Some(old) = old {
            assert_eq!(old.user_id, new.user_id);
            if let Some(channel) = old.channel_id {
                if let Some((&table, _)) = config.tables.iter().find(|(_, &c)| c == channel) {
                    let table_members = voice.members.entry(table).or_insert_with(HashSet::new);
                    table_members.remove(&old.user_id);
                }
            }
        }
        if let Some(channel) = new.channel_id {
            if let Some((&table, _)) = config.tables.iter().find(|(_, &c)| c == channel) {
                let table_members = voice.members.entry(table).or_insert_with(HashSet::new);
                table_members.insert(new.user_id);
            }
        }

        println!("voice state: {:?}", voice.members);
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let (config, mut redis) = {
            let data = ctx.data.read().await;
            (data.config(), data.redis())
        };

        if let Interaction::ApplicationCommand(command) = interaction {
            if command.guild_id != Some(config.guild_id) {
                return;
            }

            let content = match command.data.name.as_str() {
                "ping" => "Hey, I'm alive!".to_string(),
                "id" => {
                    let options = command
                        .data
                        .options
                        .get(0)
                        .expect("Expected user option")
                        .resolved
                        .as_ref()
                        .expect("Expected user object");

                    if let ApplicationCommandInteractionDataOptionValue::User(user, _member) =
                        options
                    {
                        format!("{}'s id is {}", user.tag(), user.id)
                    } else {
                        "Please provide a valid user".to_string()
                    }
                }
                "tjoin" => {
                    if config.blessed_users.contains(&command.user.id) {
                        let (table_id, team_id) = match command.data.options.as_slice() {
                            [op1, op2] => (op1.clone(), op2.clone()),
                            _ => unreachable!(),
                        };

                        let table_id = table_id.value.unwrap().as_i64().unwrap();
                        let team_id = team_id.value.unwrap().as_i64().unwrap();

                        let (in_, out, err) = table_join(&ctx, team_id, table_id).await;

                        format!(
                            "Done team {} -> table {}: {} people in, {} people out, {} people failed",
                            team_id, table_id, in_, out, err
                        )
                    } else {
                        "You should be blessed by the gods to do this".to_string()
                    }
                }
                "tkick" => {
                    if config.blessed_users.contains(&command.user.id) {
                        let table_id = match command.data.options.as_slice() {
                            [op1] => op1.clone(),
                            _ => unreachable!(),
                        };
                        let table_id = table_id.value.unwrap().as_i64().unwrap();

                        let (in_, out, err) = table_join(&ctx, DUMMY_TEAM, table_id).await;

                        format!(
                            "Done kicking from table {}: {} people in, {} people out, {} people failed",
                            table_id, in_, out, err
                        )
                    } else {
                        "You should be blessed by the gods to do this".to_string()
                    }
                }
                "queue" => {
                    let q1 = get_queue(&config, &mut redis, 1).await;
                    let q2 = get_queue(&config, &mut redis, 2).await;

                    format!(
                        "Table 1:\n{}\n\nTable 2:\n{}",
                        format_queue(&config, &q1),
                        format_queue(&config, &q2)
                    )
                }
                "enqueue" => {
                    if config.blessed_users.contains(&command.user.id) {
                        let table_id = match command.data.options.as_slice() {
                            [op1] => op1.clone(),
                            _ => unreachable!(),
                        };
                        let user = table_id.resolved.unwrap();

                        if let ApplicationCommandInteractionDataOptionValue::User(user, _member) =
                            user
                        {
                            let team_id = config.user_team(user.id);
                            if let Some(team_id) = team_id {
                                let table_id = config.team_table(team_id);

                                if add_to_queue(&config, &mut redis, table_id, team_id).await {
                                    let queue = get_queue(&config, &mut redis, table_id).await;
                                    format!(
                                        "Enqueued to table {}\n{}",
                                        table_id,
                                        format_queue(&config, &queue)
                                    )
                                } else {
                                    "They are already enqueued somewhere!".to_string()
                                }
                            } else {
                                "Cannot determine the team of the user".to_string()
                            }
                        } else {
                            "Please provide a valid user".to_string()
                        }
                    } else {
                        "You should be blessed by the gods to do this".to_string()
                    }
                }
                "enqueue_me" => {
                    let team = config.user_team(command.user.id);

                    match team {
                        None => "Cannot determine your team. Are you a participant?".to_string(),
                        Some(team) => {
                            let table_id = config.team_table(team);

                            if add_to_queue(&config, &mut redis, table_id, team).await {
                                let queue = get_queue(&config, &mut redis, table_id).await;
                                format!(
                                    "Enqueued to table {}\n{}",
                                    table_id,
                                    format_queue(&config, &queue)
                                )
                            } else {
                                "You are already enqueued somewhere!".to_string()
                            }
                        }
                    }
                }
                "enqueue_me_faster" => {
                    let team = config.user_team(command.user.id);

                    match team {
                        None => "Cannot determine your team. Are you a participant?".to_string(),
                        Some(team) => {
                            let table_id = add_to_smallest_queue(&config, &mut redis, team).await;
                            if let Some(table_id) = table_id {
                                let queue = get_queue(&config, &mut redis, table_id).await;
                                format!(
                                    "Enqueued to table {}\n{}",
                                    table_id,
                                    format_queue(&config, &queue)
                                )
                            } else {
                                "You are already enqueued somewhere!".to_string()
                            }
                        }
                    }
                }
                "unqueue_me" => {
                    //
                    let team = config.user_team(command.user.id);

                    match team {
                        None => "Cannot determine your team. Are you a participant?".to_string(),
                        Some(team) => {
                            let table_id = unqueue(&config, &mut redis, team).await;

                            if let Some(table_id) = table_id {
                                format!("Successfully unqueued from table {}", table_id)
                            } else {
                                "You don't seem to be enqueued!".to_string()
                            }
                        }
                    }
                }
                "dequeue" => {
                    if config.blessed_users.contains(&command.user.id) {
                        let table_id = match command.data.options.as_slice() {
                            [op1] => op1.clone(),
                            _ => unreachable!(),
                        };

                        let table_id = table_id.value.unwrap().as_i64().unwrap();
                        let team_id = dequeue(&config, &mut redis, table_id).await;

                        if let Some(team_id) = team_id {
                            let (in_, out, err) = table_join(&ctx, team_id, table_id).await;

                            format!(
                                "Deqeued team {} -> table {}: {} people in, {} people out, {} people failed",
                                team_id, table_id, in_, out, err
                            )
                        } else {
                            format!("The queue for table {} is empty", table_id)
                        }
                    } else {
                        "You should be blessed by the gods to do this".to_string()
                    }
                }
                "dequeue_nojoin" => {
                    if config.blessed_users.contains(&command.user.id) {
                        let table_id = match command.data.options.as_slice() {
                            [op1] => op1.clone(),
                            _ => unreachable!(),
                        };

                        let table_id = table_id.value.unwrap().as_i64().unwrap();
                        let team_id = dequeue(&config, &mut redis, table_id).await;

                        if let Some(team_id) = team_id {
                            format!("Deqeued team {} -> /dev/null", team_id)
                        } else {
                            format!("The queue for table {} is empty", table_id)
                        }
                    } else {
                        "You should be blessed by the gods to do this".to_string()
                    }
                }
                _ => "not implemented :(".to_string(),
            };

            if let Err(why) = command
                .create_interaction_response(&ctx.http, |response| {
                    response
                        .kind(InteractionResponseType::ChannelMessageWithSource)
                        .interaction_response_data(|message| message.content(content))
                })
                .await
            {
                println!("Cannot respond to slash command: {}", why);
            }
        }
    }
}

struct ConfigKey;

impl TypeMapKey for ConfigKey {
    type Value = Arc<Config>;
}

struct TableVoiceState {
    pub members: HashMap<i64, HashSet<UserId>>,
}

impl TypeMapKey for TableVoiceState {
    type Value = Self;
}

struct RedisConnKey;

impl TypeMapKey for RedisConnKey {
    type Value = MultiplexedConnection;
}

#[allow(unused)]
fn prod_config() -> Arc<Config> {
    let teams = Vec::from([
        (DUMMY_TEAM, HashSet::new(), ChannelId(952884153827885117)),
        (
            // волновая что-то там
            1,
            HashSet::from([
                UserId(418486856410202122), /* ArtemSBulgakov#4366 */
                UserId(306113516522307587), /* Данил Дубяга#0612 */
            ]),
            ChannelId(951851992093962260),
        ),
        (
            // future gadget
            2,
            HashSet::from([
                UserId(531852139262377984), /* Vekshin Arseny#8438 */
                UserId(694190982022955060), /* snowy#9316 */
                UserId(759419298212216854), /* Сапрыгин Игорь#5994 */
            ]),
            ChannelId(951879383633772605),
        ),
        (
            // команда Б
            3,
            HashSet::from([
                UserId(702202171088961556), /* Vladislav#8261 */
                UserId(885147305831989299), /* Клюкин Александр#6291 */
            ]),
            ChannelId(951851768680153118),
        ),
        (
            // шелезяка
            4,
            HashSet::from([
                UserId(301247761209360385), /* lemopsone#1936 */
                UserId(237181815973085185), /* TVerB#7415 */
                UserId(365920311688036365), /* Egorushka#3268 */
            ]),
            ChannelId(952185897992990751),
        ),
        (
            // PythonVSc
            5,
            HashSet::from([
                UserId(900079911056855040), /* Гутор Елизавета#3990 */
                UserId(504667829463941130), /* ivanmironov#0735 */
                UserId(690915090999935056), /* Тимур Ступин#6393 */
            ]),
            ChannelId(951879222224367656),
        ),
        (
            // team gazebo
            6,
            HashSet::from([
                UserId(511157552734797834), /* Fet#5017 */
                UserId(934479210590908459), /* Матвеев Роман Н#9716 */
                UserId(606083910602063872), /* Vadik prog#2484 */
            ]),
            ChannelId(952071024369872896),
        ),
        (
            // team skills building
            7,
            HashSet::from([
                UserId(690896591648981003), /* Savin_Anatolyi#9726 */
                UserId(696261215873400862), /* whatis_love#2776 */
                UserId(691935292604809236), /* Туркия Георгий#6249 */
            ]),
            ChannelId(952185981413507082),
        ),
        (
            // team name
            8,
            HashSet::from([
                UserId(640621404148203566), /* Keine Ratte#0920 */
                UserId(492763298333196298), /* RobotoLev#0880 */
                UserId(577939594822025229), /* Ver_Nick#8335 */
            ]),
            ChannelId(951879112933400676),
        ),
        (
            // безынтиллектуальная
            9,
            HashSet::from([
                UserId(559926713346162688), /* Kiri111enz#6813 */
                UserId(461761081539428363), /* GuestKP#4577 */
                UserId(441870970895073300), /* Raz0ne#5454 */
            ]),
            ChannelId(951851550081429514),
        ),
        (
            // ушки газебы
            10,
            HashSet::from([
                UserId(543072830552801280), /* MaksimZuykov#3498 */
                UserId(568165784719982604), /* Эмиль Давлитьяров#1622 */
            ]),
            ChannelId(951852461604356147),
        ),
        (
            // closedcv
            11,
            HashSet::from([
                UserId(510848390981222400), /* Краснов Андрей Алексеевич#0677 */
                UserId(534011922534629386), /* Ростов Николай#7461 */
            ]),
            ChannelId(952188351295926302),
        ),
        (
            // [REDACTED]
            12,
            HashSet::from([]),
            ChannelId(952884153827885117),
        ),
    ]);

    Arc::new(Config {
        token: include_str!("discord_token.txt").to_string(),
        app_id: 952960685787193374,
        guild_id: GuildId(951447764837990400),
        tables: HashMap::from([
            (DUMMY_TABLE, ChannelId(952884153827885117)),
            (1, ChannelId(952984861306662942)),
            (2, ChannelId(952984899734868048)),
        ]),
        participants: teams.iter().flat_map(|f| f.1.clone()).collect(),
        teams: teams.iter().map(|(k, v, _)| (*k, v.clone())).collect(),
        user_teams: teams
            .iter()
            .flat_map(|(team, uu, _)| uu.iter().map(|u| (*u, *team)))
            .collect(),
        team_voice_rooms: teams.iter().map(|(k, _, r)| (*k, *r)).collect(),
        team_names: HashMap::from([
            (DUMMY_TEAM, "Dummy team".to_string()),
            (1, "Волновая Интерференция".to_string()),
            (2, "Future Gadget Lab v4".to_string()),
            (3, "КомандаБ".to_string()),
            (4, "Шелезяка".to_string()),
            (5, "PythonVSc--".to_string()),
            (6, "Gazebo one love".to_string()),
            (7, "Skills Building".to_string()),
            (8, "team-name".to_string()),
            (9, "Безынтеллектуальные человеческие единицы".to_string()),
            (10, "Ушки Газебы".to_string()),
            (11, "ClosedCV".to_string()),
            (12, "[REDACTED]".to_string()),
        ]),
        blessed_users: HashSet::from([
            UserId(352172373774041102), /* DCNick3 */
            UserId(246300672734003211), /* Vyacheslav */
            UserId(952881541325930518), /* streamer1#0167 */
            UserId(743031905200635925), /* ipipos57#8231 */
        ]),
        environment: "prod".to_string(),
    })
}
#[allow(unused)]
fn test_config() -> Arc<Config> {
    let teams = HashMap::from([
        (DUMMY_TEAM, HashSet::new()),
        (1, HashSet::from([UserId(352172373774041102) /* DCNick3 */])),
        (
            2,
            HashSet::from([UserId(246300672734003211)] /* Vyacheslav */),
        ),
        (3, HashSet::new()),
        (4, HashSet::new()),
        (5, HashSet::new()),
        (6, HashSet::new()),
        (7, HashSet::new()),
        (8, HashSet::new()),
        (9, HashSet::new()),
        (10, HashSet::new()),
        (11, HashSet::new()),
        (12, HashSet::new()),
    ]);

    Arc::new(Config {
        token: include_str!("discord_token.txt").to_string(),
        app_id: 952960685787193374,
        guild_id: GuildId(377441849134284812),
        tables: HashMap::from([
            (DUMMY_TABLE, ChannelId(452137106609799168)),
            (1, ChannelId(952970394070036582)),
            (2, ChannelId(953042225984585829)),
        ]),
        participants: teams.iter().flat_map(|f| f.1).cloned().collect(),
        teams,
        user_teams: HashMap::from([
            (UserId(352172373774041102), 1),
            (UserId(246300672734003211), 2),
        ]),
        team_voice_rooms: HashMap::from([
            (DUMMY_TEAM, ChannelId(452137106609799168)),
            (1, ChannelId(953387785845358602)),
            (2, ChannelId(953387949540638880)),
            (3, ChannelId(452137106609799168)),
            (4, ChannelId(452137106609799168)),
            (5, ChannelId(452137106609799168)),
            (6, ChannelId(452137106609799168)),
            (7, ChannelId(452137106609799168)),
            (8, ChannelId(452137106609799168)),
            (9, ChannelId(452137106609799168)),
            (10, ChannelId(452137106609799168)),
            (11, ChannelId(452137106609799168)),
            (12, ChannelId(452137106609799168)),
        ]),
        team_names: HashMap::from([
            (DUMMY_TEAM, "Dummy team".to_string()),
            (1, "team nikita".to_string()),
            (2, "team slava".to_string()),
            (3, "n/a".to_string()),
            (4, "n/a".to_string()),
            (5, "n/a".to_string()),
            (6, "n/a".to_string()),
            (7, "n/a".to_string()),
            (8, "n/a".to_string()),
            (9, "n/a".to_string()),
            (10, "n/a".to_string()),
            (11, "n/a".to_string()),
            (12, "n/a".to_string()),
        ]),
        blessed_users: HashSet::from([
            UserId(352172373774041102), /* DCNick3 */
            UserId(246300672734003211), /* Vyacheslav */
        ]),
        environment: "test".to_string(),
    })
}

#[tokio::main]
async fn main() {
    let env = std::env::var("ENV").expect("Get current ENV");

    let config = if env == "prod" {
        println!("Starting PROD!");
        prod_config()
    } else {
        println!("Starting test");
        test_config()
    };

    let framework = StandardFramework::new().group(&GENERAL_GROUP);

    // Login with a bot token from the environment
    let mut client = Client::builder(&config.token)
        .application_id(config.app_id)
        .event_handler(Handler)
        .framework(framework)
        .await
        .expect("Error creating client");

    let redis_client = redis::Client::open("redis://127.0.0.1/").unwrap();
    let con = redis_client
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();

    {
        let mut data = client.data.write().await;
        data.insert::<ConfigKey>(config.clone());
        data.insert::<TableVoiceState>(TableVoiceState {
            members: Default::default(),
        });
        data.insert::<RedisConnKey>(con);
    }

    // println!("Users = {:#?}", users);

    // start listening for events by starting a single shard
    if let Err(why) = client.start().await {
        println!("An error occurred while running the client: {:?}", why);
    }
}
