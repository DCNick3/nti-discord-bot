use std::collections::{HashMap, HashSet};
use std::default::Default;
use std::io::Read;
use std::sync::Arc;

use futures::stream::StreamExt;
use serenity::async_trait;
use serenity::builder::CreateApplicationCommandOption;
use serenity::client::bridge::gateway::event::ShardStageUpdateEvent;
use serenity::client::{Client, Context, EventHandler};
use serenity::framework::standard::{
    macros::{command, group},
    CommandResult, StandardFramework,
};
use serenity::http::GuildPagination;
use serenity::model::channel::{
    Channel, ChannelCategory, GuildChannel, Message, PartialGuildChannel, Reaction, StageInstance,
};
use serenity::model::event::{
    ChannelPinsUpdateEvent, GuildMemberUpdateEvent, GuildMembersChunkEvent, InviteCreateEvent,
    InviteDeleteEvent, MessageUpdateEvent, PresenceUpdateEvent, ResumedEvent, ThreadListSyncEvent,
    ThreadMembersUpdateEvent, TypingStartEvent, VoiceServerUpdateEvent,
};
use serenity::model::gateway::{Presence, Ready};
use serenity::model::guild::{
    Emoji, Guild, GuildUnavailable, Integration, Member, PartialGuild, Role, ThreadMember,
};
use serenity::model::id::{
    ApplicationId, ChannelId, EmojiId, GuildId, IntegrationId, MessageId, RoleId, UserId,
};
use serenity::model::interactions::application_command::{
    ApplicationCommand, ApplicationCommandInteractionDataOptionValue, ApplicationCommandOptionType,
};
use serenity::model::interactions::{Interaction, InteractionResponseType};
use serenity::model::prelude::{CurrentUser, User, VoiceState};
use serenity::prelude::{TypeMap, TypeMapKey};

const DUMMY_TEAM: i64 = -1;
const DUMMY_TABLE: i64 = 0;

struct Config {
    pub token: String,
    pub app_id: u64,
    pub guild_id: GuildId,
    pub tables: HashMap<i64, ChannelId>,
    pub teams: HashMap<i64, Vec<UserId>>,
    pub participants: HashSet<UserId>,
    pub blessed_users: HashSet<UserId>,
}

#[group]
struct General;

struct Handler;

async fn table_join(ctx: &Context, team_id: i64, table_id: i64) -> (usize, usize) {
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

    let mut people_in = 0;
    let mut people_out = 0;
    for u in to_kick {
        people_out += 1;
        guild.move_member(&ctx, u, neutral_channel).await.unwrap();
    }
    for u in to_join {
        people_in += 1;
        guild.move_member(&ctx, u, table).await.unwrap();
    }

    (people_in, people_out)
}

// #[async_trait]
trait ContextExt {
    fn config(&self) -> Arc<Config>;
    fn voice_state(&self) -> &TableVoiceState;
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
                            .create_option(|option| {
                                option
                                    .name("table")
                                    .description("A table id (1 or 2)")
                                    .kind(ApplicationCommandOptionType::Integer)
                                    .add_int_choice("Table #1", 1)
                                    .add_int_choice("Table #2", 2)
                                    .required(true)
                            })
                            .create_option(|option| {
                                option
                                    .name("team_id")
                                    .description("Team id (0-10)")
                                    .kind(ApplicationCommandOptionType::Integer)
                                    .min_int_value(0)
                                    .max_int_value(10)
                                    .required(true)
                            })
                    })
                    .create_application_command(|command| {
                        command
                            .name("tkick")
                            .description("Kick the team currently at the table")
                            .create_option(|option| {
                                option
                                    .name("table")
                                    .description("A table id (1 or 2)")
                                    .kind(ApplicationCommandOptionType::Integer)
                                    .add_int_choice("Table #1", 1)
                                    .add_int_choice("Table #2", 2)
                                    .required(true)
                            })
                    })
            })
            .await
            .unwrap();

        // println!(
        //     "I now have the following guild slash commands: {:#?}",
        //     commands
        // );

        let guild_command =
            ApplicationCommand::create_global_application_command(&ctx.http, |command| {
                command
                    .name("wonderful_command")
                    .description("An amazing command")
            })
            .await;

        println!(
            "I created the following global slash command: {:#?}",
            guild_command
        );
    }

    async fn guild_create(&self, ctx: Context, guild: Guild, _is_new: bool) {
        let config = {
            let data = ctx.data.read().await;
            data.config()
        };

        let mut data = ctx.data.write().await;
        let voice = data.get_mut::<TableVoiceState>().unwrap();

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

        println!("{:#?}", voice.members);
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

        println!("{:#?}", voice.members);
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let config = {
            let data = ctx.data.read().await;
            data.config()
        };

        if let Interaction::ApplicationCommand(command) = interaction {
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

                        let (in_, out) = table_join(&ctx, team_id, table_id).await;

                        format!("Done! {} people in, {} people out", in_, out)
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

                        let (in_, out) = table_join(&ctx, DUMMY_TEAM, table_id).await;

                        format!("Done! {} people in, {} people out", in_, out)
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

#[tokio::main]
async fn main() {
    let teams = HashMap::from([
        (DUMMY_TEAM, Vec::new()),
        (0, Vec::from([UserId(352172373774041102) /* DCNick3 */])),
        (1, Vec::new()),
        (2, Vec::new()),
        (3, Vec::new()),
        (4, Vec::new()),
        (5, Vec::new()),
        (6, Vec::new()),
        (7, Vec::new()),
        (8, Vec::new()),
        (9, Vec::new()),
        (10, Vec::new()),
    ]);

    let config = Arc::new(Config {
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
        blessed_users: HashSet::from([UserId(352172373774041102) /* DCNick3 */]),
    });

    let framework = StandardFramework::new() // set the bot's prefix to "~"
        .group(&GENERAL_GROUP);

    // Login with a bot token from the environment
    let mut client = Client::builder(&config.token)
        .application_id(config.app_id)
        .event_handler(Handler)
        .framework(framework)
        .await
        .expect("Error creating client");

    {
        let mut data = client.data.write().await;
        data.insert::<ConfigKey>(config.clone());
        data.insert::<TableVoiceState>(TableVoiceState {
            members: Default::default(),
        })
    }

    // println!("Users = {:#?}", users);

    // start listening for events by starting a single shard
    if let Err(why) = client.start().await {
        println!("An error occurred while running the client: {:?}", why);
    }
}
