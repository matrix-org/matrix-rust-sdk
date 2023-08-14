macro_rules! handle_incoming {
    {
        chain = { $action:ident($msg:ident) -> $handler:ident -> $resp:ident },
        $($req_type:ident ($req_expr:expr) -> $incoming_type:ident ($resp_expr:expr) ),* ,
    } => {
        match $action {
            $(
                FromWidgetAction::$req_type(mut $msg) => {
                    if $msg.response.is_some() {
                        return Err(Error::InvalidJSON);
                    }

                    let (req, resp) = Request::new($req_expr);
                    $handler.handle(Incoming::$incoming_type(req)).await?;
                    let response = match resp.await.map_err(|_| Error::NoReply)? {
                        Ok($resp) => ResponseBody::Response($resp_expr),
                        Err(err) => ResponseBody::error(err.to_string()),
                    };

                    $msg.response = Some(response);
                    FromWidgetAction::$req_type($msg)
                }
            )*
        }
    }
}
